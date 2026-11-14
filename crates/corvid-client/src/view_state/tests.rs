use super::*;
use corvid_protocol::snapshot::{SNAPSHOT_CAPABILITY, SnapshotAssembler};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    LayoutNodeState, SnapshotBegin, SnapshotChunk, SnapshotEnd, SnapshotEntity, SplitAxis,
    SplitState, TabState, WorkspaceState,
};
use corvid_protocol::{PeerRole, accept_hello, hello};

fn view(target: Target) -> SavedView {
    SavedView {
        target,
        host_id: Uuid::new_v4(),
        location: Location {
            workspace: Uuid::new_v4(),
            tab: Uuid::new_v4(),
            pane: Uuid::new_v4(),
        },
    }
}

fn selection(view: &SavedView) -> ViewSelection {
    ViewSelection {
        workspace_id: view.location.workspace.to_string(),
        tab_id: view.location.tab.to_string(),
        pane_id: view.location.pane.to_string(),
    }
}

#[test]
fn clean_shutdown_flushes_the_latest_view_and_separates_presentations() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let desktop = ViewStore::new(fixture.path().join("desktop"));
    let tui = ViewStore::new(fixture.path().join("tui"));
    let first = view(Target::Local);
    let latest = view(Target::Ssh {
        alias: "build".into(),
    });
    let terminal = view(Target::Tui {
        runtime: fixture.path().join("runtime"),
    });
    let recorder = desktop.recorder()?;
    recorder.record(
        first.target.clone(),
        &first.host_id.to_string(),
        &selection(&first),
    );
    recorder.record(
        latest.target.clone(),
        &latest.host_id.to_string(),
        &selection(&latest),
    );
    let tui_recorder = tui.recorder()?;
    tui_recorder.record(
        terminal.target.clone(),
        &terminal.host_id.to_string(),
        &selection(&terminal),
    );
    drop(recorder);
    drop(tui_recorder);
    assert_eq!(desktop.last(), Some(latest));
    assert_eq!(tui.last(), Some(terminal));
    Ok(())
}

#[test]
fn concurrent_clients_merge_other_hosts_and_keep_last_selection_per_target() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let store = ViewStore::new(fixture.path().join("views"));
    let first = view(Target::Local);
    let second = view(Target::Ssh {
        alias: "build".into(),
    });
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    thread::scope(|scope| {
        for item in [&first, &second] {
            let barrier = barrier.clone();
            let store = &store;
            scope.spawn(move || {
                barrier.wait();
                store.save(item).unwrap();
            });
        }
    });
    assert_eq!(store.for_target(&first.target), Some(first.clone()));
    assert_eq!(store.for_target(&second.target), Some(second.clone()));
    let next = view(first.target.clone());
    store.save(&next)?;
    assert_eq!(store.last(), Some(next));
    assert_eq!(store.for_target(&second.target), Some(second));
    Ok(())
}

#[test]
fn corrupt_state_recovers_from_backup_and_preserves_the_damaged_bytes() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let store = ViewStore::new(fixture.path().join("views"));
    let first = view(Target::Local);
    store.save(&first)?;
    store.save(&view(Target::Local))?;
    let directory = Directory::open(&store.path)?;
    directory.replace_private_file(FILE, b"{broken")?;
    assert_eq!(store.last(), Some(first));
    let next = view(Target::Local);
    store.save(&next)?;
    assert_eq!(store.last(), Some(next));
    assert_eq!(
        directory.read_private_file("view.json.corrupt", MAX_BYTES)?,
        Some(b"{broken".to_vec())
    );
    Ok(())
}

#[test]
fn unknown_schema_and_oversized_files_are_not_overwritten() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let store = ViewStore::new(fixture.path().join("views"));
    let directory = Directory::create_all(&store.path)?;
    for bytes in [
        br#"{"schema_version":2,"views":[]}"#.to_vec(),
        vec![b' '; MAX_BYTES as usize + 1],
    ] {
        directory.replace_private_file(FILE, &bytes)?;
        assert!(store.last().is_none());
        assert!(store.save(&view(Target::Local)).is_err());
        assert_eq!(
            directory.read_private_file(FILE, MAX_BYTES + 1)?,
            Some(bytes)
        );
    }
    Ok(())
}

#[test]
fn invalid_targets_and_ids_are_not_loaded() {
    let record = view(Target::Local);
    let mut stored = serde_json::to_value(Stored {
        schema_version: 1,
        views: vec![record],
    })
    .unwrap();
    stored["views"][0]["host_id"] = "not-an-id".into();
    assert!(decode(&serde_json::to_vec(&stored).unwrap()).is_err());
    assert!(
        Target::Ssh {
            alias: "-oProxyCommand=bad".into()
        }
        .validate()
        .is_err()
    );
    assert!(
        Target::Tui {
            runtime: "relative".into()
        }
        .validate()
        .is_err()
    );
}

fn workspace(saved: &SavedView) -> WorkspaceState {
    WorkspaceState {
        workspace_id: saved.location.workspace.to_string(),
        label: "Test workspace".into(),
        tabs: vec![TabState {
            tab_id: saved.location.tab.to_string(),
            label: "Test tab".into(),
            nodes: vec![LayoutNodeState {
                body: Some(Node::PaneId(saved.location.pane.to_string())),
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

fn snapshot(host: Uuid, workspaces: Vec<WorkspaceState>) -> SynchronizedSnapshot {
    let session = accept_hello(
        &hello(
            "hello".into(),
            Some(Uuid::new_v4().to_string()),
            vec![SNAPSHOT_CAPABILITY.into()],
        ),
        PeerRole::Control,
        &host.to_string(),
        &Uuid::new_v4().to_string(),
        &[SNAPSHOT_CAPABILITY],
    )
    .unwrap()
    .0;
    let mut assembler = SnapshotAssembler::new(session.clone(), "snapshot".into(), false);
    let mut result = None;
    for body in [
        Body::SnapshotBegin(SnapshotBegin {
            chunks: 1,
            capabilities: session.capabilities.clone(),
        }),
        Body::SnapshotChunk(SnapshotChunk {
            index: 0,
            entities: workspaces
                .into_iter()
                .map(|workspace| SnapshotEntity {
                    body: Some(Entity::Workspace(workspace)),
                })
                .collect(),
            ..Default::default()
        }),
        Body::SnapshotEnd(SnapshotEnd {}),
    ] {
        let mut frame = session.envelope(Some("snapshot".into()), body);
        frame.sequence = Some(0);
        result = assembler.push(frame).unwrap();
    }
    result.unwrap()
}

#[test]
fn restores_exact_ids_without_requiring_a_live_terminal() {
    let saved = view(Target::Local);
    let state = snapshot(saved.host_id, vec![workspace(&saved)]);
    assert_eq!(saved.resolve(&state).unwrap(), selection(&saved));
    let wrong_host = snapshot(Uuid::new_v4(), vec![workspace(&saved)]);
    assert_eq!(
        saved
            .resolve(&wrong_host)
            .unwrap_err()
            .downcast_ref::<ProtocolFault>()
            .unwrap()
            .code,
        "HostMismatch"
    );
}

#[test]
fn deleted_pane_falls_back_within_its_tab_and_deleted_tab_within_its_workspace() {
    let saved = view(Target::Local);
    let mut layout = workspace(&saved);
    let first = Uuid::new_v4().to_string();
    let second = Uuid::new_v4().to_string();
    layout.tabs[0].nodes = vec![
        LayoutNodeState {
            body: Some(Node::Split(SplitState {
                axis: SplitAxis::Horizontal as i32,
                children: vec![1, 2],
            })),
            ..Default::default()
        },
        LayoutNodeState {
            body: Some(Node::PaneId(first.clone())),
            ..Default::default()
        },
        LayoutNodeState {
            body: Some(Node::PaneId(second)),
            ..Default::default()
        },
    ];
    let resolved = saved
        .resolve(&snapshot(saved.host_id, vec![layout.clone()]))
        .unwrap();
    assert_eq!(resolved.tab_id, saved.location.tab.to_string());
    assert_eq!(resolved.pane_id, first);
    layout.tabs[0].tab_id = Uuid::new_v4().to_string();
    let resolved = saved
        .resolve(&snapshot(saved.host_id, vec![layout.clone()]))
        .unwrap();
    assert_eq!(resolved.tab_id, layout.tabs[0].tab_id);
    assert_eq!(resolved.workspace_id, saved.location.workspace.to_string());
    let unrelated = workspace(&view(Target::Local));
    assert_eq!(
        saved
            .resolve(&snapshot(saved.host_id, vec![unrelated]))
            .unwrap_err()
            .downcast_ref::<ProtocolFault>()
            .unwrap()
            .code,
        "SavedWorkspaceGone"
    );
}
