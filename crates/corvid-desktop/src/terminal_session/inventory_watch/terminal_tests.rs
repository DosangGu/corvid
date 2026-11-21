use super::*;
use corvid_protocol::wire::Detach;

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires the isolated deploy/test_title_ssh.py fixture"]
async fn ssh_workspace_stops_retrying_and_manually_reopens_the_same_terminal() -> Result<()> {
    let root = PathBuf::from(std::env::var_os("CORVID_SSH_TEST_ROOT").context("SSH fixture root")?);
    let alias = SshAlias::new(std::env::var("CORVID_TITLE_TEST_SSH_ALIAS")?)?;
    let state = root.join("desktop");
    let start = |target, explicit| {
        Session::start(
            Destination::Ssh(alias.clone()),
            state.clone(),
            ViewStore::new(state.join("views")),
            target,
            explicit,
            ParkedConnections::default(),
        )
    };
    let disconnect = || -> Result<()> {
        std::fs::write(root.join("offline"), "")?;
        let pid = std::fs::read_to_string(root.join("writer-pid"))?;
        ensure!(
            std::process::Command::new("kill")
                .arg(pid.trim())
                .status()?
                .success()
        );
        Ok(())
    };
    let (session, job, _prompts, _activations) = start(AttachTarget::Default, false)?;
    let original = wait_for_ssh_ui(&session, |ui| ui.connected && ui.inventory_ready).await?;
    let recovered = {
        // Keep the daemon-side transport open while the local SSH process exits.
        let _relay = StoppedRelay::stop(&root)?;
        disconnect()?;
        wait_for_ssh_ui(&session, |ui| !ui.connected && ui.changing).await?;
        std::fs::remove_file(root.join("offline"))?;
        wait_for_ssh_ui(&session, |ui| ui.connected && ui.inventory_ready).await?
    };
    assert_eq!(recovered.terminal_id, original.terminal_id);
    assert_eq!(recovered.selected, original.selected);

    disconnect()?;
    let offline = wait_for_ssh_ui(&session, |ui| ui.ssh_offline).await?;
    assert!(offline.message.contains("Automatic reconnect stopped"));
    job.join();
    let attempts = std::fs::read(root.join("attempts"))?;
    std::fs::remove_file(root.join("offline"))?;
    corvid_async::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(std::fs::read(root.join("attempts"))?, attempts);

    let target = offline
        .reconnect_target
        .context("missing manual reconnect target")?;
    let (session, mut job, _prompts, _activations) = start(target, true)?;
    let recovered = wait_for_ssh_ui(&session, |ui| ui.connected && ui.inventory_ready).await?;
    assert_eq!(recovered.terminal_id, original.terminal_id);
    assert_eq!(recovered.selected, original.selected);
    job.stop(false);
    job.join();
    Ok(())
}

#[cfg(target_os = "linux")]
struct StoppedRelay(String);

#[cfg(target_os = "linux")]
impl StoppedRelay {
    fn stop(root: &Path) -> Result<Self> {
        let pid = std::fs::read_to_string(root.join("relay-pid"))?;
        ensure!(pid.parse::<u32>()? > 1, "invalid SSH fixture relay PID");
        ensure!(
            std::process::Command::new("kill")
                .args(["-STOP", &pid])
                .status()?
                .success(),
            "could not stop the remote relay"
        );
        Ok(Self(pid))
    }
}

#[cfg(target_os = "linux")]
impl Drop for StoppedRelay {
    fn drop(&mut self) {
        let _ = std::process::Command::new("kill")
            .args(["-CONT", &self.0])
            .status();
    }
}

#[cfg(target_os = "linux")]
async fn wait_for_ssh_ui(session: &Session, matches: impl Fn(&Ui) -> bool) -> Result<Ui> {
    let mut changes = session.changes();
    timeout(Duration::from_secs(75), async {
        loop {
            let ui = session.ui();
            if matches(&ui) {
                return Ok(ui);
            }
            changes.changed().await?;
        }
    })
    .await
    .with_context(|| format!("waiting for SSH workspace: {}", session.ui().message))?
}

struct Daemon {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<Result<()>>>,
    _directory: tempfile::TempDir,
}

impl Daemon {
    async fn start() -> Result<(Self, ControlEndpoint)> {
        let directory = tempfile::tempdir()?;
        let runtime = directory.path().join("runtime");
        let state = directory.path().join("state");
        let endpoint = ControlEndpoint::Local(runtime.clone());
        let (stop, stopped) = oneshot::channel();
        let daemon_runtime = runtime.clone();
        let thread = thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(async {
                    let shutdown = async {
                        let _ = stopped.await;
                    };
                    #[cfg(windows)]
                    {
                        corvid_daemon::run(&daemon_runtime, &state, shutdown).await
                    }
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    {
                        corvid_daemon::run(&daemon_runtime, &state, false, shutdown).await
                    }
                })
        });
        let daemon = Self {
            stop: Some(stop),
            thread: Some(thread),
            _directory: directory,
        };
        timeout(Duration::from_secs(10), async {
            while corvid_client::ping(&runtime).await.is_err() {
                corvid_async::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        Ok((daemon, endpoint))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap().unwrap();
        }
    }
}

fn title_program() -> LaunchCommand {
    #[cfg(windows)]
    let args = [
        "powershell.exe",
        "-NoLogo",
        "-NoProfile",
        "-Command",
        "while ($null -ne ($title = [Console]::ReadLine())) { $Host.UI.RawUI.WindowTitle = $title }",
    ];
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let args = [
        "/bin/sh",
        "-c",
        r#"stty -echo; while IFS= read -r title; do printf '\033]2;%s\007' "$title"; done"#,
    ];
    LaunchCommand {
        argv: args.iter().map(|arg| arg.as_bytes().to_vec()).collect(),
        ..Default::default()
    }
}

async fn open(
    endpoint: &ControlEndpoint,
    target: Option<ViewSelection>,
) -> Result<(InteractiveConnection, Attached)> {
    let mut link = endpoint
        .interactive(vec![
            "attach.v1".into(),
            "render.v1".into(),
            GRACEFUL_DETACH_CAPABILITY.into(),
            corvid_protocol::PARK_TERMINAL_CAPABILITY.into(),
        ])
        .await?;
    let request = if let Some(target) = target {
        Attach {
            workspace_id: Some(target.workspace_id),
            tab_id: Some(target.tab_id),
            pane_id: Some(target.pane_id),
            ..Default::default()
        }
    } else {
        Attach {
            create_workspace: Some("Title regression".into()),
            launch: Some(title_program()),
            ..Default::default()
        }
    };
    let attached = attach(&mut link, Grid::new(24, 80), false, request).await?;
    Ok((link, attached))
}

fn title(shared: &Shared, pane: &str) -> Option<String> {
    let buffer = shared.snapshot();
    if !buffer.ui.inventory_ready {
        return None;
    }
    buffer
        .ui
        .inventory
        .as_ref()?
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .flat_map(|tab| &tab.nodes)
        .find(|node| matches!(&node.body, Some(Node::PaneId(id)) if id == pane))
        .map(|node| node.title.clone())
}

async fn wait_title(
    link: &mut InteractiveConnection,
    shared: &Shared,
    pane: &str,
    expected: &str,
) -> Result<()> {
    let mut changes = shared.ui_changed.subscribe();
    timeout(Duration::from_secs(5), async {
        loop {
            if title(shared, pane).as_deref() == Some(expected) {
                return Ok(());
            }
            corvid_async::select! {
                changed = changes.changed() => {
                    changed?;
                }
                frame = link.receive() => {
                    if let Frame::Render(frame) = frame? {
                        link.send(Body::FrameApplied(FrameApplied {
                            render_generation: frame.header.render_generation,
                            frame_id: frame.header.frame_id,
                        })).await?;
                    }
                }
            }
        }
    })
    .await
    .context("waiting for daemon title publication")?
}

async fn verify_titles(endpoint: ControlEndpoint) -> Result<()> {
    let (mut link, attached) = open(&endpoint, None).await?;
    let target = attached.selection.context("missing selection")?;
    let route = attached.route_token.context("missing route")?;
    let shared = Arc::new(Shared::new(&Destination::Local, session_runtime().unwrap()));
    let watcher = InventoryWatch::start(
        endpoint.clone(),
        link.session().host_id.clone(),
        link.session().server_epoch.clone(),
        Arc::clone(&shared),
    )?;
    let mut elapsed = Vec::new();
    for sequence in 1..=32 {
        let expected = format!("Title-{sequence}");
        let started = Instant::now();
        let newline = if cfg!(windows) { '\r' } else { '\n' };
        link.send(Body::InputChunk(InputChunk {
            route_token: Some(route.clone()),
            input_sequence: sequence,
            data: format!("{expected}{newline}").into_bytes(),
        }))
        .await?;
        wait_title(&mut link, &shared, &target.pane_id, &expected).await?;
        elapsed.push(started.elapsed());
    }
    elapsed.sort_unstable();
    eprintln!(
        "PTY input to Desktop title, 32 updates: p50={:?}, p95={:?}, max={:?}",
        elapsed[16], elapsed[30], elapsed[31]
    );
    assert!(
        elapsed[30] < Duration::from_secs(1),
        "title delivery repeatedly stalled"
    );
    link.send(Body::Detach(Detach {})).await?;
    drop(link);
    drop(watcher);

    // No new OSC is emitted on reattach: the daemon's recorded title is sufficient.
    let (mut link, resumed) = open(&endpoint, Some(target.clone())).await?;
    ensure!(resumed.route_token.as_ref().unwrap().terminal_id == route.terminal_id);
    let shared = Arc::new(Shared::new(&Destination::Local, session_runtime().unwrap()));
    let _watcher = InventoryWatch::start(
        endpoint,
        link.session().host_id.clone(),
        link.session().server_epoch.clone(),
        Arc::clone(&shared),
    )?;
    wait_title(&mut link, &shared, &target.pane_id, "Title-32").await?;
    link.send(Body::Detach(Detach {})).await?;
    Ok(())
}

#[tokio::test]
async fn parked_transport_releases_its_writer_and_reattaches_with_a_fresh_route() -> Result<()> {
    let (_daemon, endpoint) = Daemon::start().await?;
    let (mut link, attached) = open(&endpoint, None).await?;
    let view = attached.selection.context("missing initial pane")?;
    let original = attached.route_token.context("missing initial route")?;

    let id = link
        .send(Body::GracefulDetach(
            corvid_protocol::wire::GracefulDetach {
                keep_connection: true,
            },
        ))
        .await?;
    timeout(COMMAND_TIMEOUT, async {
        loop {
            let frame = link.receive().await?;
            if let Frame::Structured(_) = &frame {
                let (request, body) = envelope(&link, frame)?;
                if request.as_ref() == Some(&id) {
                    ensure!(
                        matches!(body, Body::Pong(_)),
                        "daemon did not park the transport"
                    );
                    return Ok::<_, anyhow::Error>(());
                }
            }
        }
    })
    .await??;

    // An independent client can attach without taking over the parked pane.
    let (mut other, attached) = open(&endpoint, Some(view.clone())).await?;
    let other_route = attached.route_token.context("missing second route")?;
    assert_eq!(other_route.terminal_id, original.terminal_id);
    let id = other
        .send(Body::GracefulDetach(
            corvid_protocol::wire::GracefulDetach::default(),
        ))
        .await?;
    timeout(COMMAND_TIMEOUT, async {
        loop {
            let frame = other.receive().await?;
            if let Frame::Structured(_) = &frame {
                let (request, body) = envelope(&other, frame)?;
                if request.as_ref() == Some(&id) {
                    ensure!(
                        matches!(body, Body::Detach(_)),
                        "second client did not detach"
                    );
                    return Ok::<_, anyhow::Error>(());
                }
            }
        }
    })
    .await??;

    let cache = ParkedConnections::default();
    cache.start(crate::terminal_session::session_runtime()?);
    let host = link.session().host_id.clone();
    let epoch = link.session().server_epoch.clone();
    cache.insert(Destination::Local, endpoint, link).await;
    let (_, mut link) = cache
        .take(&Destination::Local, Some(&host), Some(&epoch))
        .await
        .context("could not recover the parked transport")?;
    let reattached = attach(
        &mut link,
        Grid::new(24, 80),
        false,
        Attach {
            workspace_id: Some(view.workspace_id.clone()),
            tab_id: Some(view.tab_id.clone()),
            pane_id: Some(view.pane_id.clone()),
            ..Default::default()
        },
    )
    .await?;
    let route = reattached.route_token.context("missing resumed route")?;
    assert_eq!(reattached.selection, Some(view));
    assert_eq!(route.terminal_id, original.terminal_id);
    assert_ne!(route.client_id, original.client_id);
    assert!(route.connection_generation > other_route.connection_generation);
    Ok(())
}

#[tokio::test]
async fn pty_titles_reach_desktop_and_survive_reattach_without_new_output() -> Result<()> {
    let (_daemon, endpoint) = Daemon::start().await?;
    verify_titles(endpoint).await
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires a disposable loopback SSH daemon and CORVID_TITLE_TEST_SSH_ALIAS"]
async fn ssh_pty_titles_reach_desktop_and_survive_reattach() -> Result<()> {
    let alias = SshAlias::new(std::env::var("CORVID_TITLE_TEST_SSH_ALIAS")?)?;
    verify_titles(ControlEndpoint::Ssh(alias)).await
}
