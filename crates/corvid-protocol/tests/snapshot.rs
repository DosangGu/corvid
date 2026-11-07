use corvid_protocol::snapshot::{
    EntityKey, SnapshotAssembler, SynchronizedSnapshot, WATCH_CAPABILITY,
};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::layout_node_state::Body as NodeBody;
use corvid_protocol::wire::snapshot_entity::Body as EntityBody;
use corvid_protocol::wire::{
    ClientState, Envelope, LayoutNodeState, SnapshotBegin, SnapshotChunk, SnapshotDelta,
    SnapshotEnd, SnapshotEntity, TabState, TerminalState, ViewSelection, Viewport, WatchReady,
    WorkspaceState, WriterLeaseState,
};
use corvid_protocol::{PeerRole, Session, accept_hello, hello};

const BASE: u64 = (1 << 53) + 40;

fn session() -> Session {
    let hello = hello(
        "greeting".into(),
        Some("controller".into()),
        vec![WATCH_CAPABILITY.into()],
    );
    accept_hello(
        &hello,
        PeerRole::Control,
        "host",
        "epoch",
        &[WATCH_CAPABILITY],
    )
    .unwrap()
    .0
}

fn base(session: &Session, body: Body) -> Envelope {
    let mut frame = session.envelope(Some("snapshot-request".into()), body);
    frame.subscription_id = Some("watch-id".into());
    frame.sequence = Some(BASE);
    frame
}

fn event(session: &Session, sequence: u64, body: Body) -> Envelope {
    let mut frame = session.envelope(None, body);
    frame.subscription_id = Some("watch-id".into());
    frame.sequence = Some(sequence);
    frame
}

fn workspace() -> SnapshotEntity {
    SnapshotEntity {
        body: Some(EntityBody::Workspace(WorkspaceState {
            workspace_id: "workspace".into(),
            label: "Example".into(),
            tabs: vec![TabState {
                tab_id: "tab".into(),
                label: "Example tab".into(),
                nodes: vec![LayoutNodeState {
                    body: Some(NodeBody::PaneId("pane".into())),
                }],
            }],
        })),
    }
}

fn location() -> ViewSelection {
    ViewSelection {
        workspace_id: "workspace".into(),
        tab_id: "tab".into(),
        pane_id: "pane".into(),
    }
}

fn terminal() -> SnapshotEntity {
    SnapshotEntity {
        body: Some(EntityBody::Terminal(TerminalState {
            terminal_id: "terminal".into(),
            location: Some(location()),
        })),
    }
}

fn client() -> SnapshotEntity {
    SnapshotEntity {
        body: Some(EntityBody::Client(ClientState {
            client_id: "client".into(),
            connection_generation: 2,
            terminal_id: Some("terminal".into()),
            selection: Some(location()),
            view_revision: Some(1),
            viewport: Some(Viewport { cols: 80, rows: 24 }),
            writer_leases: vec![WriterLeaseState {
                workspace_id: "workspace".into(),
                generation: 2,
            }],
        })),
    }
}

fn staged(session: &Session) -> SnapshotAssembler {
    let mut assembler = SnapshotAssembler::new(session.clone(), "snapshot-request".into(), true);
    let begin = SnapshotBegin {
        chunks: 1,
        capabilities: session.capabilities.clone(),
    };
    let chunk = SnapshotChunk {
        index: 0,
        entities: vec![workspace()],
    };
    for body in [
        Body::SnapshotBegin(begin),
        Body::SnapshotChunk(chunk),
        Body::SnapshotEnd(SnapshotEnd {}),
    ] {
        assert!(assembler.push(base(session, body)).unwrap().is_none());
    }
    assembler
}

fn ready(session: &Session) -> SynchronizedSnapshot {
    staged(session)
        .push(event(session, BASE, Body::WatchReady(WatchReady {})))
        .unwrap()
        .unwrap()
}

fn attach(session: &Session, sequence: u64) -> Envelope {
    event(
        session,
        sequence,
        Body::SnapshotDelta(SnapshotDelta {
            upsert: vec![client(), terminal()],
            remove: Vec::new(),
        }),
    )
}

#[test]
fn staged_snapshot_applies_atomic_deltas_through_the_exact_watermark() {
    let session = session();
    let mut assembler = staged(&session);
    assert!(
        assembler
            .push(attach(&session, BASE + 1))
            .unwrap()
            .is_none()
    );

    let snapshot = assembler
        .push(event(&session, BASE + 1, Body::WatchReady(WatchReady {})))
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.snapshot().sequence(), BASE + 1);
    assert_eq!(snapshot.snapshot().entities().count(), 3);
    assert_eq!(snapshot.subscription_id(), Some("watch-id"));
}

#[test]
fn missing_chunks_and_unapplied_watermarks_cannot_publish_ready() {
    let session = session();
    let mut assembler = SnapshotAssembler::new(session.clone(), "snapshot-request".into(), true);
    let begin = SnapshotBegin {
        chunks: 2,
        capabilities: session.capabilities.clone(),
    };
    assert!(
        assembler
            .push(base(&session, Body::SnapshotBegin(begin)))
            .unwrap()
            .is_none()
    );
    let Err(fault) = assembler.push(base(&session, Body::SnapshotEnd(SnapshotEnd {}))) else {
        panic!("incomplete snapshot was accepted");
    };
    assert_eq!(fault.code, "ResyncRequired");

    let Err(fault) =
        staged(&session).push(event(&session, BASE + 1, Body::WatchReady(WatchReady {})))
    else {
        panic!("watermark was accepted before its delta");
    };
    assert_eq!(fault.code, "ResyncRequired");
}

#[test]
fn duplicate_sequences_are_idempotent_and_gaps_invalidate_the_watch() {
    let session = session();
    let mut cache = ready(&session);
    assert!(cache.apply(attach(&session, BASE + 1)).unwrap());
    assert!(!cache.apply(attach(&session, BASE + 1)).unwrap());
    let fault = cache.apply(attach(&session, BASE + 3)).unwrap_err();
    assert_eq!(fault.code, "ResyncRequired");
    assert!(!cache.is_valid());
    assert_eq!(cache.snapshot().sequence(), BASE + 1);
    assert_eq!(cache.snapshot().entities().count(), 3);
}

#[test]
fn a_delta_cannot_remove_a_terminal_still_referenced_by_a_client() {
    let session = session();
    let mut cache = ready(&session);
    assert!(cache.apply(attach(&session, BASE + 1)).unwrap());
    let reference = EntityKey::from_entity(&terminal()).unwrap().reference();
    let removal = SnapshotDelta {
        upsert: Vec::new(),
        remove: vec![reference],
    };
    let fault = cache
        .apply(event(&session, BASE + 2, Body::SnapshotDelta(removal)))
        .unwrap_err();
    assert_eq!(fault.code, "ResyncRequired");
    assert!(!cache.is_valid());
    assert_eq!(cache.snapshot().sequence(), BASE + 1);
    assert!(
        cache
            .snapshot()
            .entities()
            .any(|entity| *entity == terminal())
    );
}

#[test]
fn events_from_another_epoch_leave_the_previous_cache_intact() {
    let session = session();
    let mut cache = ready(&session);
    let mut next = attach(&session, BASE + 1);
    next.server_epoch = Some("different-epoch".into());
    assert_eq!(cache.apply(next).unwrap_err().code, "EpochMismatch");
    assert!(!cache.is_valid());
    assert_eq!(cache.snapshot().sequence(), BASE);
    assert_eq!(cache.snapshot().entities().count(), 1);
}
