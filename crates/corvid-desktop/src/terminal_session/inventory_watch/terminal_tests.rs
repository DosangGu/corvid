use super::*;
use corvid_protocol::wire::Detach;

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
                tokio::time::sleep(Duration::from_millis(10)).await;
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
        .interactive(vec!["attach.v1".into(), "render.v1".into()])
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
    let buffer = shared.buffer();
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
            tokio::select! {
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
    let shared = Arc::new(Shared::new(&Destination::Local));
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
    let shared = Arc::new(Shared::new(&Destination::Local));
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
