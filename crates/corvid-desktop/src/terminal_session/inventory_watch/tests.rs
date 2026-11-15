use super::*;
use corvid_protocol::snapshot::WATCH_CAPABILITY;
use corvid_protocol::wire::{
    Envelope, LayoutNodeState, Ping, SnapshotBegin, SnapshotChunk, SnapshotDelta, SnapshotEnd,
    SnapshotEntity, TabState, WatchReady,
};
use corvid_protocol::{FrameReader, FrameRole, FrameWriter, PeerRole, accept_hello};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncRead, AsyncWrite};

enum Update {
    Title(usize, String),
    Delay(Duration),
    Disconnect,
    ChangeEpoch,
}

struct Server {
    endpoint: ControlEndpoint,
    host: String,
    epoch: String,
    updates: mpsc::UnboundedSender<Update>,
    connections: Arc<AtomicUsize>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<Result<()>>>,
    _directory: tempfile::TempDir,
}

impl Server {
    fn start() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("runtime");
        let endpoint = ControlEndpoint::Local(path.clone());
        let host = Uuid::new_v4().to_string();
        let epoch = Uuid::new_v4().to_string();
        let (updates, receive) = mpsc::unbounded_channel();
        let (stop, stopped) = oneshot::channel();
        let (ready, receive_ready) = std::sync::mpsc::channel();
        let connections = Arc::new(AtomicUsize::new(0));
        let model = Model {
            host: host.clone(),
            epoch: epoch.clone(),
            sequence: 0,
            titles: ["Initial A".into(), "Initial B".into()],
            connections: Arc::clone(&connections),
        };
        let thread = thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(async {
                    tokio::select! {
                        _ = stopped => Ok(()),
                        result = serve(&path, model, receive, ready) => result,
                    }
                })
        });
        receive_ready.recv_timeout(Duration::from_secs(5))??;
        Ok(Self {
            endpoint,
            host,
            epoch,
            updates,
            connections,
            stop: Some(stop),
            thread: Some(thread),
            _directory: directory,
        })
    }

    fn watch(&self, shared: &Arc<Shared>) -> Result<InventoryWatch> {
        InventoryWatch::start(
            self.endpoint.clone(),
            self.host.clone(),
            self.epoch.clone(),
            Arc::clone(shared),
        )
    }

    fn send(&self, update: Update) {
        self.updates.send(update).unwrap();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap().unwrap();
        }
    }
}

struct Model {
    host: String,
    epoch: String,
    sequence: u64,
    titles: [String; 2],
    connections: Arc<AtomicUsize>,
}

impl Model {
    fn entity(&self) -> SnapshotEntity {
        SnapshotEntity {
            body: Some(EntityBody::Workspace(WorkspaceState {
                workspace_id: "workspace".into(),
                label: "Workspace".into(),
                tabs: self
                    .titles
                    .iter()
                    .enumerate()
                    .map(|(index, title)| TabState {
                        tab_id: format!("tab-{index}"),
                        label: title.clone(),
                        nodes: vec![LayoutNodeState {
                            title: title.clone(),
                            body: Some(Node::PaneId(format!("pane-{index}"))),
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                    .collect(),
            })),
        }
    }
}

async fn serve(
    path: &Path,
    mut model: Model,
    mut updates: mpsc::UnboundedReceiver<Update>,
    ready: std::sync::mpsc::Sender<Result<()>>,
) -> Result<()> {
    #[cfg(windows)]
    let mut listener = corvid_protocol::windows::DaemonPipe::Control
        .bind(&corvid_protocol::windows::StateDirectory::create_all(path)?)?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let directory = corvid_protocol::unix::RuntimeDir::create_all(path)?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let instance = directory.lock_instance()?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let listener = instance
        .bind(corvid_protocol::unix::SocketRole::Control)
        .await?;
    ready.send(Ok(())).unwrap();
    loop {
        #[cfg(windows)]
        let stream = listener.accept().await?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let stream = listener.accept().await?.context("rejected test peer")?;
        model.connections.fetch_add(1, Ordering::SeqCst);
        // Disconnects and identity rejection are expected fixture operations.
        let _ = serve_watch(stream, &mut model, &mut updates).await;
    }
}

async fn serve_watch(
    stream: impl AsyncRead + AsyncWrite + Unpin,
    model: &mut Model,
    updates: &mut mpsc::UnboundedReceiver<Update>,
) -> Result<()> {
    let (read, write) = tokio::io::split(stream);
    let mut writer = FrameWriter::start(write, FrameRole::StructuredOnly).await?;
    let mut reader = FrameReader::start(read, FrameRole::StructuredOnly).await?;
    let greeting = reader.next_envelope().await?.context("missing hello")?;
    let (session, reply) = accept_hello(
        &greeting,
        PeerRole::Control,
        &model.host,
        &model.epoch,
        &[WATCH_CAPABILITY],
    )?;
    writer.envelope(&reply).await?;
    let request = reader.next_envelope().await?.context("missing watch")?;
    ensure!(matches!(request.body, Some(Body::SnapshotWatch(_))));
    let subscription = Uuid::new_v4().to_string();
    let frame = |sequence, body, request_id| {
        let mut frame = session.envelope(request_id, body);
        frame.subscription_id = Some(subscription.clone());
        frame.sequence = Some(sequence);
        frame
    };
    for body in [
        Body::SnapshotBegin(SnapshotBegin {
            chunks: 1,
            capabilities: session.capabilities.clone(),
        }),
        Body::SnapshotChunk(SnapshotChunk {
            entities: vec![model.entity()],
            ..Default::default()
        }),
        Body::SnapshotEnd(SnapshotEnd {}),
    ] {
        writer
            .envelope(&frame(model.sequence, body, request.request_id.clone()))
            .await?;
    }
    writer
        .envelope(&frame(
            model.sequence,
            Body::WatchReady(WatchReady {}),
            None,
        ))
        .await?;
    loop {
        // Framed reads cannot be cancelled between chunks when an update wins.
        let pending_read = reader.next_envelope();
        tokio::pin!(pending_read);
        loop {
            tokio::select! {
                update = updates.recv() => match update.context("fixture stopped")? {
                    Update::Title(pane, title) => {
                        model.titles[pane] = title;
                        model.sequence += 1;
                        let delta = Body::SnapshotDelta(SnapshotDelta {
                            upsert: vec![model.entity()],
                            ..Default::default()
                        });
                        writer.envelope(&frame(model.sequence, delta, None)).await?;
                    }
                    Update::Delay(delay) => tokio::time::sleep(delay).await,
                    Update::Disconnect => {
                        model.titles[0] = "Changed while offline".into();
                        model.sequence += 1;
                        return Ok(());
                    }
                    Update::ChangeEpoch => {
                        model.epoch = Uuid::new_v4().to_string();
                        model.titles[0] = "Wrong epoch".into();
                        return Ok(());
                    }
                },
                request = &mut pending_read => {
                    let Envelope { request_id, body, .. } = request?.context("client disconnected")?;
                    ensure!(matches!(body, Some(Body::Ping(Ping {}))));
                    let response = session.envelope(request_id, Body::Pong(corvid_protocol::wire::Pong {}));
                    writer.envelope(&response).await?;
                    break;
                }
            }
        }
    }
}

fn has_title(shared: &Shared, pane: usize, title: &str) -> bool {
    let buffer = shared.buffer();
    buffer.ui.inventory_ready
        && buffer.ui.inventory.as_ref().is_some_and(|inventory| {
            inventory.workspaces[0].tabs[pane].label == title
                && inventory.workspaces[0].tabs[pane].nodes[0].title == title
        })
}

async fn wait_for(shared: &Shared, ready: impl Fn() -> bool) {
    let mut changes = shared.ui_changed.subscribe();
    timeout(Duration::from_secs(5), async {
        while !ready() {
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn titles_keep_up_with_bursts_and_reconnect_to_the_latest_snapshot() -> Result<()> {
    let server = Server::start()?;
    let shared = Arc::new(Shared::new(&Destination::Local));
    let watcher = server.watch(&shared)?;
    wait_for(&shared, || has_title(&shared, 0, "Initial A")).await;
    let started = Instant::now();
    for sequence in 0..128 {
        server.send(Update::Title(0, format!("A {sequence}")));
        server.send(Update::Title(1, format!("B {sequence}")));
    }
    wait_for(&shared, || {
        has_title(&shared, 0, "A 127") && has_title(&shared, 1, "B 127")
    })
    .await;
    assert_eq!(
        server.connections.load(Ordering::SeqCst),
        1,
        "burst reconnected the watch"
    );
    eprintln!("256 title changes applied in {:?}", started.elapsed());

    server.send(Update::Disconnect);
    wait_for(&shared, || has_title(&shared, 0, "Changed while offline")).await;
    assert!(has_title(&shared, 1, "B 127"));
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);
    server.send(Update::ChangeEpoch);
    wait_for(&shared, || !shared.buffer().ui.inventory_ready).await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(!shared.buffer().ui.inventory_ready);
    assert_eq!(
        shared.buffer().ui.inventory.as_ref().unwrap().workspaces[0].tabs[0].label,
        "Changed while offline"
    );

    let stopped = Instant::now();
    drop(watcher);
    assert!(stopped.elapsed() < Duration::from_secs(1));
    let connections = server.connections.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(
        server.connections.load(Ordering::SeqCst),
        connections,
        "retired watch reconnected"
    );
    Ok(())
}

#[tokio::test]
async fn title_publication_progresses_while_the_render_thread_is_blocked() -> Result<()> {
    let server = Server::start()?;
    let shared = Arc::new(Shared::new(&Destination::Local));
    let _watcher = server.watch(&shared)?;
    wait_for(&shared, || has_title(&shared, 0, "Initial A")).await;
    let started = Instant::now();
    server.send(Update::Title(0, "While rendering".into()));

    // Blocking this entire current-thread runtime simulates synchronous ANSI
    // parsing. Metadata must still reach Shared without polling the writer loop.
    while !has_title(&shared, 0, "While rendering") && started.elapsed() < Duration::from_secs(1) {
        thread::sleep(Duration::from_millis(1));
    }
    assert!(has_title(&shared, 0, "While rendering"));
    eprintln!(
        "Title applied with render thread blocked in {:?}",
        started.elapsed()
    );
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn delayed_title_delivery_does_not_add_a_retry_or_polling_interval() -> Result<()> {
    let server = Server::start()?;
    let shared = Arc::new(Shared::new(&Destination::Local));
    let _watcher = server.watch(&shared)?;
    wait_for(&shared, || has_title(&shared, 0, "Initial A")).await;
    let delay = Duration::from_millis(40);
    let mut samples = Vec::new();
    for index in 0..20 {
        let title = format!("Delayed {index}");
        let started = Instant::now();
        server.send(Update::Delay(delay));
        server.send(Update::Title(0, title.clone()));
        wait_for(&shared, || has_title(&shared, 0, &title)).await;
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    eprintln!(
        "40ms delayed title stream: p50={:?}, p95={:?}",
        samples[10], samples[18]
    );
    assert!(
        samples[18] < Duration::from_millis(250),
        "title delivery added substantial delay"
    );
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
    Ok(())
}
