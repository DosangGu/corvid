use super::*;
use crate::connection::Connection;
use corvid_protocol::wire::layout_node_state::Body as Node;
use corvid_protocol::wire::snapshot_entity::Body as Entity;
use corvid_protocol::wire::{
    LayoutNodeState, SnapshotBegin, SnapshotChunk, SnapshotDelta, SnapshotEnd, SnapshotEntity,
    TabState, WatchReady, WorkspaceState,
};
use corvid_protocol::{
    FrameReader, FrameRole, FrameWriter, PeerRole, Session, accept_hello, hello,
};
use tokio::io::{DuplexStream, ReadHalf, WriteHalf, duplex, split};

#[cfg(feature = "compio-endpoints")]
use compio::{runtime::spawn, time::sleep};
#[cfg(not(feature = "compio-endpoints"))]
use tokio::{spawn, time::sleep};

fn run<F: std::future::Future<Output = ()>>(test: F) {
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    #[cfg(feature = "compio-endpoints")]
    {
        // The in-memory fixture still uses the Tokio framing codec.
        let _fixture_context = tokio.enter();
        compio::runtime::Runtime::new().unwrap().block_on(test);
    }
    #[cfg(not(feature = "compio-endpoints"))]
    tokio.block_on(test);
}

const SUBSCRIPTION: &str = "title-watch";
const BURST: u64 = 256;

fn session() -> Session {
    accept_hello(
        &hello(
            "hello".into(),
            Some("controller".into()),
            vec![WATCH_CAPABILITY.into()],
        ),
        PeerRole::Control,
        "host",
        "epoch",
        &[WATCH_CAPABILITY],
    )
    .unwrap()
    .0
}

fn workspace(title: &str) -> SnapshotEntity {
    SnapshotEntity {
        body: Some(Entity::Workspace(WorkspaceState {
            workspace_id: "workspace".into(),
            label: "Workspace".into(),
            tabs: vec![TabState {
                tab_id: "tab".into(),
                label: title.into(),
                nodes: vec![LayoutNodeState {
                    title: title.into(),
                    body: Some(Node::PaneId("pane".into())),
                    ..Default::default()
                }],
                ..Default::default()
            }],
        })),
    }
}

fn event(session: &Session, sequence: u64, body: Body) -> Envelope {
    let mut frame = session.envelope(None, body);
    frame.subscription_id = Some(SUBSCRIPTION.into());
    frame.sequence = Some(sequence);
    frame
}

fn cache(session: &Session) -> SynchronizedSnapshot {
    let mut assembler = SnapshotAssembler::new(session.clone(), "snapshot".into(), true);
    for body in [
        Body::SnapshotBegin(SnapshotBegin {
            chunks: 1,
            capabilities: session.capabilities.clone(),
        }),
        Body::SnapshotChunk(SnapshotChunk {
            entities: vec![workspace("Initial")],
            ..Default::default()
        }),
        Body::SnapshotEnd(SnapshotEnd {}),
    ] {
        let mut frame = event(session, 0, body);
        frame.request_id = Some("snapshot".into());
        assert!(assembler.push(frame).unwrap().is_none());
    }
    assembler
        .push(event(session, 0, Body::WatchReady(WatchReady {})))
        .unwrap()
        .unwrap()
}

type TestConnection = Wire<ReadHalf<DuplexStream>, WriteHalf<DuplexStream>>;

async fn connection() -> (TestConnection, FrameWriter<WriteHalf<DuplexStream>>) {
    let (client, server) = duplex(256 * 1024);
    let (read, write) = split(client);
    let (server_read, server_write) = split(server);
    let server = FrameWriter::start(server_write, FrameRole::StructuredOnly)
        .await
        .unwrap();
    let writer = FrameWriter::start(write, FrameRole::StructuredOnly)
        .await
        .unwrap();
    let reader = FrameReader::start(read, FrameRole::StructuredOnly)
        .await
        .unwrap();
    // Keep the server's read half alive through its writer half's shared stream.
    drop(server_read);
    (
        Connection {
            session: session(),
            reader,
            writer,
        }
        .into(),
        server,
    )
}

async fn send_titles(writer: &mut FrameWriter<WriteHalf<DuplexStream>>, count: u64) {
    for sequence in 1..=count {
        let frame = event(
            &session(),
            sequence,
            Body::SnapshotDelta(SnapshotDelta {
                upsert: vec![workspace(&format!("Title {sequence}"))],
                ..Default::default()
            }),
        );
        writer.envelope(&frame).await.unwrap();
    }
}

#[test]
fn title_burst_survives_a_briefly_paused_consumer_without_resync() {
    run(async {
        let (mut connection, mut writer) = connection().await;
        let mut cache = cache(&connection.session);
        let (send, mut receive) = mpsc::channel(CACHE_QUEUE);
        let task = spawn(async move { pump(&mut connection, SUBSCRIPTION, send).await });
        send_titles(&mut writer, BURST).await;
        sleep(Duration::from_millis(100)).await;
        assert!(
            !task.is_finished(),
            "a short title burst disconnected the watch"
        );
        assert_eq!(receive.len(), CACHE_QUEUE);

        watch_timeout(Duration::from_secs(2), async {
            for sequence in 1..=BURST {
                assert!(cache.apply(receive.recv().await.unwrap()).unwrap());
                assert_eq!(cache.snapshot().sequence(), sequence);
            }
        })
        .await
        .unwrap();
        assert_eq!(
            cache.snapshot().entities().next().unwrap(),
            &workspace("Title 256")
        );
        assert!(!task.is_finished());
        #[cfg(not(feature = "compio-endpoints"))]
        task.abort();
    });
}

#[test]
fn a_stalled_consumer_is_bounded_and_requires_resync() {
    run(async {
        let (mut connection, mut writer) = connection().await;
        let (send, receive) = mpsc::channel(CACHE_QUEUE);
        let task = spawn(async move { pump(&mut connection, SUBSCRIPTION, send).await });
        send_titles(&mut writer, BURST).await;
        let error = watch_timeout(COMMAND_TIMEOUT + Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ProtocolFault>().unwrap().code,
            "ResyncRequired"
        );
        assert_eq!(receive.len(), CACHE_QUEUE);
    });
}
