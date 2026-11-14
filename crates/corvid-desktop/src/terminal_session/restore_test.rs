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
        #[cfg(target_os = "linux")]
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
        verify_restore(&root.join("desktop")).await?;
        verify_window_restore(&root.join("windows")).await
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

async fn verify_attach_without_agent_restore(runtime: &Path, state: &Path) -> Result<()> {
    #[cfg(windows)]
    let mut listener = corvid_protocol::windows::DaemonPipe::Client.bind(
        &corvid_protocol::windows::StateDirectory::create_all(runtime)?,
    )?;
    #[cfg(target_os = "linux")]
    let directory = corvid_protocol::unix::RuntimeDir::create_all(runtime)?;
    #[cfg(target_os = "linux")]
    let instance = directory.lock_instance()?;
    #[cfg(target_os = "linux")]
    let listener = instance
        .bind(corvid_protocol::unix::SocketRole::Client)
        .await?;

    let desktop = AttachedSession::start(state)?;
    timeout(COMMAND_TIMEOUT, async {
        #[cfg(windows)]
        let stream = listener.accept().await?;
        #[cfg(target_os = "linux")]
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
            &Uuid::new_v4().to_string(),
            &Uuid::new_v4().to_string(),
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
        let request = reader
            .next_envelope()
            .await?
            .context("missing Desktop Attach")?;
        ensure!(matches!(
            session.validate_request(&request)?,
            Body::Attach(_)
        ));
        ensure!(!desktop.session.ui().restore_supported);
        Ok(())
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
        let (session, job, _prompts) =
            Session::start(Destination::Local, state.to_owned(), store, target, false)?;
        Ok(Self {
            session,
            job: Some(job),
        })
    }
}

impl Drop for AttachedSession {
    fn drop(&mut self) {
        if let Some(mut job) = self.job.take() {
            job.stop();
            job.join();
        }
    }
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

async fn verify_restore(state: &Path) -> Result<()> {
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
    #[cfg(target_os = "linux")]
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
    drop(first);

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
    #[cfg(target_os = "linux")]
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

async fn verify_window_restore(state: &Path) -> Result<()> {
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
