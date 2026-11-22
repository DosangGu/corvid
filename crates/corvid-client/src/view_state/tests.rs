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
fn keeps_each_workspace_from_a_coalesced_batch_and_reads_legacy_views() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let path = fixture.path().join("views");
    let store = ViewStore::new(path.clone());
    let first = view(Target::Local);
    let mut second = view(first.target.clone());
    second.host_id = first.host_id;
    let directory = Directory::create_all(&path)?;
    directory.replace_private_file(
        FILE,
        &serde_json::to_vec(&serde_json::json!({ "schema_version": 1, "views": [&first] }))?,
    )?;
    let host = first.host_id.to_string();
    let first_id = first.location.workspace.to_string();
    let second_id = second.location.workspace.to_string();
    assert_eq!(
        store.for_workspace(&first.target, &host, &first_id),
        Some(first.clone())
    );
    store.save(&second)?;
    assert_eq!(
        store.for_workspace(&first.target, &host, &first_id),
        Some(first.clone())
    );

    let recorder = store.recorder()?;
    let mut updated = first;
    updated.location.pane = Uuid::new_v4();
    recorder.record(updated.target.clone(), &host, &selection(&updated));
    recorder.record(second.target.clone(), &host, &selection(&second));
    drop(recorder);

    assert_eq!(store.last(), Some(second.clone()));
    assert_eq!(
        store.for_workspace(&updated.target, &host, &first_id),
        Some(updated)
    );
    assert_eq!(
        store.for_workspace(&second.target, &host, &second_id),
        Some(second.clone())
    );
    assert!(
        store
            .for_workspace(&second.target, &Uuid::new_v4().to_string(), &second_id)
            .is_none()
    );
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

#[cfg(windows)]
#[test]
fn concurrent_save_waits_for_a_slow_windows_writer() -> Result<()> {
    use std::sync::mpsc;
    use std::time::Instant;

    let fixture = tempfile::tempdir()?;
    let store = ViewStore::new(fixture.path().join("views"));
    let directory = Directory::create_all(&store.path)?;
    let first = view(Target::Local);
    let second = view(Target::Ssh {
        alias: "build".into(),
    });
    let lock = directory.lock_storage()?;
    let acquired = Instant::now();
    let (started, waiting) = mpsc::channel();

    thread::scope(|scope| -> Result<()> {
        let store = &store;
        let second = &second;
        let waiter = scope.spawn(move || {
            let started_at = Instant::now();
            started.send(())?;
            store
                .save(second)
                .with_context(|| format!("second writer failed after {:?}", started_at.elapsed()))
        });
        waiting.recv_timeout(Duration::from_secs(2))?;
        // Exceed the old 500 ms retry budget while holding the usual exclusive lock.
        thread::sleep(Duration::from_millis(850));
        let saved = ViewStore::save_locked(&directory, std::slice::from_ref(&first));
        drop(lock);
        let held_for = acquired.elapsed();
        saved.context("saving while holding the original Windows storage lock")?;
        waiter
            .join()
            .unwrap()
            .with_context(|| format!("first writer held the lock for {held_for:?}"))?;
        Ok(())
    })?;

    assert_eq!(store.for_target(&first.target), Some(first));
    assert_eq!(store.last(), Some(second));
    Ok(())
}

#[cfg(windows)]
#[test]
fn windows_save_reports_lock_timeout_and_preserves_the_saved_view() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let store = ViewStore::new(fixture.path().join("views"));
    let original = view(Target::Local);
    store.save(&original)?;
    let directory = Directory::open(&store.path)?;
    let lock = directory.lock_storage()?;

    let error = store.save(&view(Target::Local)).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("waiting for private Windows state lock"),
        "{error:#}"
    );
    let io_error = error.downcast_ref::<std::io::Error>().unwrap();
    assert_eq!(
        io_error.raw_os_error(),
        Some(windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32)
    );

    drop(lock);
    assert_eq!(store.last(), Some(original));
    assert!(directory.read_private_file(BACKUP, MAX_BYTES)?.is_none());
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
        views: vec![record],
        ..Stored::default()
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
            body: Some(Node::PaneId(second.clone())),
            ..Default::default()
        },
    ];
    let resolved = saved
        .resolve(&snapshot(saved.host_id, vec![layout.clone()]))
        .unwrap();
    assert_eq!(resolved.tab_id, saved.location.tab.to_string());
    assert_eq!(resolved.pane_id, first);
    assert_eq!(
        saved
            .preferred_pane(&layout, |view| view.pane_id == second)
            .unwrap()
            .pane_id,
        second,
    );
    assert!(saved.preferred_pane(&layout, |_| false).is_none());
    layout.tabs[0].tab_id = Uuid::new_v4().to_string();
    assert!(saved.preferred_pane(&layout, |_| true).is_none());
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
