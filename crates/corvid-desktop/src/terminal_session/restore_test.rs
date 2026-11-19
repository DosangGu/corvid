use super::*;
use corvid_protocol::{FrameReader, FrameRole, FrameWriter, PeerRole, accept_hello};
use std::path::Path;

#[test]
fn desktop_relaunch_restores_the_same_terminal() -> Result<()> {
    isolated_fixture(
        "terminal_session::restore_test::desktop_relaunch_restores_the_same_terminal",
        run_fixture,
    )
}

#[test]
fn desktop_attaches_without_agent_restore_support() -> Result<()> {
    isolated_fixture(
        "terminal_session::restore_test::desktop_attaches_without_agent_restore_support",
        async |root| {
            verify_attach_without_agent_restore(&root.join("runtime"), &root.join("desktop")).await
        },
    )
}

#[test]
fn desktop_opening_a_workspace_offers_recovery_before_opening_a_shell() -> Result<()> {
    isolated_fixture(
        "terminal_session::restore_test::desktop_opening_a_workspace_offers_recovery_before_opening_a_shell",
        verify_recovery_offers,
    )
}

// Run the backend in a subprocess so environment overrides never affect other tests.
fn isolated_fixture(test: &str, run: impl AsyncFnOnce(&Path) -> Result<()>) -> Result<()> {
    if let Some(root) = std::env::var_os("CORVID_DESKTOP_RESTORE_FIXTURE") {
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(run(Path::new(&root)));
    }
    let fixture = tempfile::Builder::new()
        .prefix("corvid-desktop-view-test-")
        .tempdir()?;
    let output = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", test, "--nocapture"])
        .env("CORVID_DESKTOP_RESTORE_FIXTURE", fixture.path())
        .env("CORVID_RUNTIME_DIR", fixture.path().join("runtime"))
        .output()?;
    ensure!(
        output.status.success(),
        "Desktop fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

async fn run_fixture(root: &Path) -> Result<()> {
    with_daemon(root, async |runtime| {
        verify_restore(runtime, &root.join("desktop")).await?;
        verify_window_restore(runtime, &root.join("windows")).await
    })
    .await
}

async fn with_daemon(root: &Path, run: impl AsyncFnOnce(&Path) -> Result<()>) -> Result<()> {
    let runtime = root.join("runtime");
    let state = root.join("daemon");
    let (stop, stopped) = oneshot::channel();
    let server_runtime = runtime.clone();
    let server = tokio::spawn(async move {
        let shutdown = async {
            let _ = stopped.await;
        };
        #[cfg(windows)]
        {
            corvid_daemon::run(&server_runtime, &state, shutdown).await
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            corvid_daemon::run(&server_runtime, &state, false, shutdown).await
        }
    });
    let result = async {
        timeout(Duration::from_secs(15), async {
            while corvid_client::ping(&runtime).await.is_err() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("starting the fixture daemon")?;
        run(&runtime).await
    }
    .await;
    let _ = stop.send(());
    let stopped = timeout(Duration::from_secs(15), server)
        .await
        .context("stopping the fixture daemon");
    result?;
    stopped???;
    Ok(())
}

async fn verify_recovery_offers(root: &Path) -> Result<()> {
    use corvid_core::agent::{AgentSessionKey, Provider};
    use corvid_core::layout::Layout;
    use corvid_protocol::wire::PaneRestoreStatus;
    use serde_json::json;

    #[cfg(unix)]
    use corvid_protocol::unix::RuntimeDir as StateDirectory;
    #[cfg(windows)]
    use corvid_protocol::windows::StateDirectory;

    let mut layout = Layout::empty_terminal();
    let first = layout.primary().unwrap();
    let second = layout.add_tab(first.workspace, "Codex".into())?;
    let saved = [(first, Provider::Claude), (second, Provider::Codex)]
        .into_iter()
        .map(|(location, provider)| {
            let key = AgentSessionKey {
                provider,
                provider_session_id: "saved-id".into(),
            };
            (
                location.pane.to_string(),
                json!({ "key": key, "verified": true }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let directory = StateDirectory::create_all(&root.join("daemon"))?;
    directory.replace_private_file(
        "state.json",
        &serde_json::to_vec(&json!({
            "schema_version": 5, "layout": layout, "history": [], "resumes": saved,
        }))?,
    )?;
    with_daemon(root, async |runtime| {
        let endpoint = ControlEndpoint::Local(runtime.to_owned());
        let snapshot = endpoint.current_snapshot(None).await?;
        let host_id = snapshot.session().host_id.clone();
        let state = root.join("desktop");
        let picker = AttachTarget::Workspace {
            id: first.workspace.to_string(),
            host_id: host_id.clone(),
            new_tab: None,
        };
        let selection = ViewSelection {
            workspace_id: second.workspace.to_string(),
            tab_id: second.tab.to_string(),
            pane_id: second.pane.to_string(),
        };
        for (target, pane) in [
            (picker, first.pane),
            (AttachTarget::FreshDefault, first.pane),
            (
                AttachTarget::OpenPane {
                    view: selection.clone(),
                    host_id: host_id.clone(),
                },
                second.pane,
            ),
            (
                AttachTarget::Restore(SavedView::new(SavedTarget::Local, &host_id, &selection)?),
                second.pane,
            ),
        ] {
            let desktop =
                AttachedSession::with_target(&state, ViewStore::new(state.join("view")), target)?;
            let ui = wait_disconnected(&desktop.session).await?;
            let view = ui.selected.context("missing recovery selection")?;
            ensure!(view.pane_id == pane.to_string());
            ensure!(ui.inventory.is_some_and(|inventory| !inventory.live(&view)));
            ensure!(
                ui.restore_results.iter().any(|result| {
                    result.location.as_ref() == Some(&view)
                        && result.status() == PaneRestoreStatus::Available
                }),
                "Desktop did not expose restore for the selected pane: {}",
                ui.message
            );
            drop(desktop);
        }
        let shell = AttachedSession::with_target(
            &state,
            ViewStore::new(state.join("view")),
            AttachTarget::Pane(selection.clone(), true),
        )?;
        ensure!(wait_view(&shell.session, |_| true).await?.selected == Some(selection));
        tokio::task::spawn_blocking(move || drop(shell)).await?;
        Ok(())
    })
    .await
}

async fn verify_attach_without_agent_restore(runtime: &Path, state: &Path) -> Result<()> {
    #[cfg(windows)]
    let mut listener = corvid_protocol::windows::DaemonPipe::Client.bind(
        &corvid_protocol::windows::StateDirectory::create_all(runtime)?,
    )?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let directory = corvid_protocol::unix::RuntimeDir::create_all(runtime)?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let instance = directory.lock_instance()?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let listener = instance
        .bind(corvid_protocol::unix::SocketRole::Client)
        .await?;

    let desktop = AttachedSession::start(state)?;
    timeout(COMMAND_TIMEOUT, async {
        let host = Uuid::new_v4().to_string();
        let epoch = Uuid::new_v4().to_string();
        // The fixture exposes no upgrade socket. Desktop retries with the same daemon.
        for _ in 0..2 {
            #[cfg(windows)]
            let stream = listener.accept().await?;
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let stream = listener.accept().await?.context("rejected fixture peer")?;

            let (read, write) = tokio::io::split(stream);
            let mut writer = FrameWriter::start(write, FrameRole::TuiOutput).await?;
            let mut reader = FrameReader::start(read, FrameRole::StructuredOnly).await?;
            let hello = reader
                .next_envelope()
                .await?
                .context("missing Desktop Hello")?;
            let (session, reply) = accept_hello(
                &hello,
                PeerRole::Tui,
                &host,
                &epoch,
                &[
                    "attach.v1",
                    "render.v1",
                    "paste.v1",
                    "select_view.v1",
                    CREATE_VIEW_CAPABILITY,
                    CLAIM_WORKSPACE_CAPABILITY,
                    PANE_PREVIEW_CAPABILITY,
                    DEFAULT_SHELL_CAPABILITY,
                ],
            )?;
            writer.envelope(&reply).await?;
            if let Some(request) = reader.next_envelope().await? {
                ensure!(matches!(
                    session.validate_request(&request)?,
                    Body::Attach(_)
                ));
                ensure!(!desktop.session.ui().restore_supported);
                return Ok(());
            }
        }
        bail!("Desktop did not attach")
    })
    .await
    .context("waiting for Desktop to attach without agent restoration")?
}

struct AttachedSession {
    session: Session,
    job: Option<Job>,
}

impl AttachedSession {
    fn start(state: &Path) -> Result<Self> {
        Self::with_store(state, ViewStore::new(state.join("desktop-view")))
    }

    fn with_store(state: &Path, store: ViewStore) -> Result<Self> {
        Self::with_target(state, store, AttachTarget::Default)
    }

    fn with_target(state: &Path, store: ViewStore, target: AttachTarget) -> Result<Self> {
        Self::with_destination(state, store, target, Destination::Local)
    }

    fn with_destination(
        state: &Path,
        store: ViewStore,
        target: AttachTarget,
        destination: Destination,
    ) -> Result<Self> {
        let (session, job, _prompts, _activations) = Session::start(
            destination,
            state.to_owned(),
            store,
            target,
            false,
            ParkedConnections::default(),
        )?;
        Ok(Self {
            session,
            job: Some(job),
        })
    }
}

impl Drop for AttachedSession {
    fn drop(&mut self) {
        if let Some(mut job) = self.job.take() {
            job.stop(false);
            job.join();
        }
    }
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "requires an isolated Linux SSH daemon and CORVID_DESKTOP_TEST_SSH_ALIAS"]
async fn ssh_split_preview_and_input_survive_reattach() -> Result<()> {
    let alias = SshAlias::new(std::env::var("CORVID_DESKTOP_TEST_SSH_ALIAS")?)?;
    let fixture = tempfile::tempdir()?;
    let state = fixture.path().join("desktop");
    let store = ViewStore::new(state.join("desktop-view"));
    let start = || {
        AttachedSession::with_destination(
            &state,
            store.clone(),
            AttachTarget::Default,
            Destination::Ssh(alias.clone()),
        )
    };
    let first = start()?;
    let initial = wait_view(&first.session, |_| true).await?.selected.unwrap();
    wait_screen(&first.session, &initial.pane_id, "").await?;
    ensure!(first.session.send(
        &initial.pane_id,
        b"printf 'SSH_FIRST_%s\\n' ready\r".to_vec(),
        false
    ));
    wait_screen(&first.session, &initial.pane_id, "SSH_FIRST_ready").await?;
    first.session.create(Target::SplitAxis(
        corvid_protocol::wire::SplitAxis::Horizontal as i32,
    ));
    let split = wait_view(&first.session, |ui| {
        ui.selected
            .as_ref()
            .is_some_and(|view| view.pane_id != initial.pane_id)
    })
    .await?;
    let pane = split.selected.as_ref().unwrap();
    wait_screen(&first.session, &pane.pane_id, "").await?;

    let mut updates = first.session.changes();
    timeout(Duration::from_secs(20), async {
        loop {
            if first.session.preview(&initial.pane_id, |screen| {
                screen.contents().contains("SSH_FIRST_ready")
            }) == Some(true)
            {
                break Ok::<_, anyhow::Error>(());
            }
            updates.changed().await?;
        }
    })
    .await
    .context("waiting for the SSH split preview")??;
    ensure!(
        !first
            .session
            .send(&initial.pane_id, b"wrong pane".to_vec(), false)
    );
    ensure!(first.session.send(
        &pane.pane_id,
        b"printf 'SSH_SPLIT_%s\\n' ready\r".to_vec(),
        false
    ));
    wait_screen(&first.session, &pane.pane_id, "SSH_SPLIT_ready").await?;
    tokio::task::spawn_blocking(move || drop(first)).await?;

    let resumed = start()?;
    let restored = wait_view(&resumed.session, |_| true).await?;
    ensure!(restored.selected == split.selected && restored.terminal_id == split.terminal_id);
    wait_screen(&resumed.session, &pane.pane_id, "SSH_SPLIT_ready").await?;
    ensure!(resumed.session.send(
        &pane.pane_id,
        b"printf 'SSH_RESUMED_%s\\n' ready\r".to_vec(),
        false
    ));
    wait_screen(&resumed.session, &pane.pane_id, "SSH_RESUMED_ready").await?;
    tokio::task::spawn_blocking(move || drop(resumed)).await?;
    Ok(())
}

async fn wait_view(session: &Session, ready: impl Fn(&Ui) -> bool) -> Result<Ui> {
    let mut changes = session.changes();
    timeout(Duration::from_secs(20), async {
        loop {
            let ui = session.ui();
            if ui.connected && !ui.changing && ui.inventory_ready && ready(&ui) {
                return Ok(ui);
            }
            ensure!(ui.changing || ui.connected, "{}", ui.message);
            changes.changed().await?;
        }
    })
    .await
    .context("waiting for Desktop view")?
}

async fn wait_screen(session: &Session, pane: &str, marker: &str) -> Result<()> {
    let mut changes = session.screen_changes();
    timeout(Duration::from_secs(20), async {
        loop {
            if session.screen(pane, |screen| screen.contents().contains(marker)) == Some(true) {
                return Ok(());
            }
            changes.changed().await?;
        }
    })
    .await
    .context("waiting for Desktop terminal output")?
}

async fn wait_disconnected(session: &Session) -> Result<Ui> {
    let mut changes = session.changes();
    timeout(Duration::from_secs(20), async {
        loop {
            let ui = session.ui();
            if !ui.connected && !ui.changing {
                return Ok(ui);
            }
            changes.changed().await?;
        }
    })
    .await
    .context("waiting for Desktop detach")?
}

async fn wait_for_client_release(runtime: &Path) -> Result<()> {
    timeout(COMMAND_TIMEOUT, async {
        loop {
            if corvid_client::clients(runtime).await?.clients.is_empty() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("waiting for the fixture daemon to release Desktop clients")?
}

async fn verify_restore(runtime: &Path, state: &Path) -> Result<()> {
    let first = AttachedSession::start(state)?;
    let initial = wait_view(&first.session, |_| true).await?.selected.unwrap();
    first
        .session
        .create(Target::WorkspaceLabel("Restore workspace".into()));
    let workspace = wait_view(&first.session, |ui| {
        ui.selected
            .as_ref()
            .is_some_and(|view| view.workspace_id != initial.workspace_id)
    })
    .await?
    .selected
    .unwrap();
    first.session.create(Target::TabLabel("Restore tab".into()));
    let tab = wait_view(&first.session, |ui| {
        ui.selected
            .as_ref()
            .is_some_and(|view| view.tab_id != workspace.tab_id)
    })
    .await?
    .selected
    .unwrap();
    first.session.create(Target::SplitAxis(
        corvid_protocol::wire::SplitAxis::Horizontal as i32,
    ));
    let selected = wait_view(&first.session, |ui| {
        ui.selected
            .as_ref()
            .is_some_and(|view| view.pane_id != tab.pane_id)
    })
    .await?;
    let view = selected.selected.as_ref().unwrap();
    wait_screen(&first.session, &view.pane_id, "").await?;
    #[cfg(windows)]
    let command =
        b"$restoreValue='kept'; [Console]::WriteLine(('DESKTOP_' + $restoreValue))\r".to_vec();
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let command = b"restore_value=kept; printf 'DESKTOP_%s\\n' \"$restore_value\"\n".to_vec();
    ensure!(
        first.session.send(&view.pane_id, command, false),
        "input was not accepted"
    );
    wait_screen(&first.session, &view.pane_id, "DESKTOP_kept").await?;
    let endpoint = selected
        .endpoint
        .as_ref()
        .context("missing control endpoint")?;
    let snapshot = endpoint.current_snapshot(None).await?;
    let inventory = Inventory::read(&snapshot);
    ensure!(
        Some(&snapshot.session().host_id) == selected.host_id.as_ref()
            && inventory.live(view)
            && inventory.has_writer(view)
            && inventory.clients.len() == 1,
        "reading the workspace list changed the terminal or its writer"
    );
    let different_host = uuid::Uuid::new_v4().to_string();
    let mismatch = endpoint.current_snapshot(Some(&different_host)).await;
    ensure!(
        mismatch
            .err()
            .and_then(|error| error.downcast::<ProtocolFault>().ok())
            .is_some_and(|fault| fault.code == "HostMismatch"),
        "inventory lookup accepted an unexpected daemon"
    );
    // The test daemon shares this executor. Let it process the detach while
    // the Desktop shutdown waits for the acknowledgement on its own thread.
    tokio::task::spawn_blocking(move || drop(first)).await?;
    ensure!(
        corvid_client::clients(runtime).await?.clients.is_empty(),
        "Desktop finished detaching before the daemon released its writer"
    );

    let store = ViewStore::new(state.join("desktop-view"));
    let saved = store.last().context("Desktop did not save its view")?;
    ensure!(saved.location.pane.to_string() == view.pane_id);
    ensure!(matches!(
        Destination::from_saved(saved.target)?,
        Destination::Local
    ));
    let second = AttachedSession::start(state)?;
    let restored = wait_view(&second.session, |_| true).await?;
    ensure!(
        restored.selected == selected.selected && restored.terminal_id == selected.terminal_id,
        "Desktop changed the restored selection or terminal"
    );
    wait_screen(&second.session, &view.pane_id, "DESKTOP_kept").await?;
    #[cfg(windows)]
    let command = b"[Console]::WriteLine(('CHECK_' + $restoreValue))\r".to_vec();
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let command = b"printf 'CHECK_%s\\n' \"$restore_value\"\n".to_vec();
    ensure!(second.session.send(&view.pane_id, command, false));
    wait_screen(&second.session, &view.pane_id, "CHECK_kept").await?;
    ensure!(
        second
            .session
            .send(&view.pane_id, b"exit\r".to_vec(), false)
    );
    wait_disconnected(&second.session).await?;
    drop(second);
    wait_for_client_release(runtime).await?;

    let empty = AttachedSession::start(state)?;
    let ui = wait_disconnected(&empty.session).await?;
    ensure!(
        ui.restore_unavailable,
        "restoration unexpectedly opened a new terminal: {}",
        ui.message
    );
    ensure!(
        ui.inventory
            .as_ref()
            .is_some_and(|inventory| !inventory.live(view)),
        "the ended terminal was restarted"
    );
    ensure!(
        store
            .last()
            .is_some_and(|saved| saved.location.pane.to_string() == view.pane_id),
        "failed restoration overwrote the saved view"
    );
    drop(empty);
    let empty = AttachedSession::with_target(
        state,
        store,
        AttachTarget::ResumeAgents {
            view: view.clone(),
            host_id: selected.host_id.clone().unwrap(),
            pane_only: true,
        },
    )?;
    let ui = wait_disconnected(&empty.session).await?;
    ensure!(ui.restore_supported && ui.selected.as_ref() == Some(view));
    ensure!(
        ui.restore_results
            .iter()
            .any(|result| result.location.as_ref() == Some(view)
                && result.status() == corvid_protocol::wire::PaneRestoreStatus::Empty),
        "explicit restore did not preserve the empty shell pane"
    );
    drop(empty);
    Ok(())
}

async fn verify_window_restore(runtime: &Path, state: &Path) -> Result<()> {
    use corvid_client::view_state::windows::WindowRegistry;

    let registry = WindowRegistry::new(state.join("desktop-view"));
    let mut windows = registry.restore()?;
    windows.push(registry.create()?);
    let mut sessions = Vec::new();
    let mut expected = Vec::new();
    for (index, window) in windows.iter().enumerate() {
        let attached = AttachedSession::with_store(state, window.store.clone())?;
        let initial = wait_view(&attached.session, |_| true)
            .await?
            .selected
            .unwrap();
        attached
            .session
            .create(Target::WorkspaceLabel(format!("Window {index}")));
        let selected = wait_view(&attached.session, |ui| {
            ui.selected
                .as_ref()
                .is_some_and(|view| view.workspace_id != initial.workspace_id)
        })
        .await?;
        wait_screen(
            &attached.session,
            &selected.selected.as_ref().unwrap().pane_id,
            "",
        )
        .await?;
        expected.push((selected.selected, selected.terminal_id));
        sessions.push(attached);
    }
    drop(sessions);
    wait_for_client_release(runtime).await?;
    drop(windows);

    let windows = registry.restore()?;
    ensure!(windows.len() == 2, "missing restored windows");
    let mut sessions = Vec::new();
    for (window, (selection, terminal)) in windows.iter().zip(&expected) {
        let attached = AttachedSession::with_store(state, window.store.clone())?;
        let restored = wait_view(&attached.session, |_| true).await?;
        ensure!(&restored.selected == selection && &restored.terminal_id == terminal);
        sessions.push(attached);
    }

    // A duplicate saved selection must not displace an active restored window.
    let duplicate = AttachedSession::with_store(state, windows[0].store.clone())?;
    let rejected = wait_disconnected(&duplicate.session).await?;
    ensure!(
        rejected.restore_unavailable,
        "occupied restoration did not offer selection"
    );
    ensure!(
        sessions[0].session.ui().connected,
        "restoration displaced the existing writer"
    );
    drop(duplicate);
    let occupied = sessions[0].session.ui();
    let duplicate = AttachedSession::with_target(
        state,
        windows[0].store.clone(),
        AttachTarget::ResumeAgents {
            view: occupied.selected.clone().unwrap(),
            host_id: occupied.host_id.clone().unwrap(),
            pane_only: false,
        },
    )?;
    let rejected = wait_disconnected(&duplicate.session).await?;
    ensure!(
        rejected.restore_results.iter().any(|result| {
            result.terminal_id == occupied.terminal_id
                && result.status() == corvid_protocol::wire::PaneRestoreStatus::Existing
        }) && sessions[0].session.ui().connected,
        "agent restore displaced an existing writer"
    );
    drop(duplicate);
    drop(sessions);
    let mut windows = windows;
    registry.close(windows.remove(0))?;
    let remaining_id = windows[0].id;
    drop(windows);
    let restored = registry.restore()?;
    ensure!(restored.len() == 1 && restored[0].id == remaining_id);
    Ok(())
}
