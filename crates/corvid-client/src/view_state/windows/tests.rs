use super::*;

fn saved() -> SavedView {
    SavedView {
        target: Target::Local,
        host_id: Uuid::new_v4(),
        location: Location {
            workspace: Uuid::new_v4(),
            tab: Uuid::new_v4(),
            pane: Uuid::new_v4(),
        },
    }
}

#[test]
fn restores_independent_windows_and_skips_live_instances() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let registry = WindowRegistry::new(fixture.path().join("desktop-view"));
    let mut initial = registry.restore()?;
    let first = initial.pop().unwrap();
    let second = registry.create()?;
    let first_id = first.id;
    let second_id = second.id;
    let a = saved();
    let b = saved();
    first.store.save(&a)?;
    second.store.save(&b)?;
    assert!(registry.restore()?.is_empty());
    drop(first);
    let restored = registry.restore()?;
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].id, first_id);
    assert_eq!(restored[0].store.last(), Some(a));
    drop(restored);
    drop(second);
    let restored = registry.restore()?;
    assert_eq!(restored.len(), 2);
    assert_eq!(restored[1].id, second_id);
    assert_eq!(restored[1].store.last(), Some(b));
    Ok(())
}

#[test]
fn closing_one_window_keeps_other_windows_and_does_not_repeat_legacy_import() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let registry = WindowRegistry::new(fixture.path().join("desktop-view"));
    let legacy = ViewStore::new(registry.path.clone());
    let old = saved();
    legacy.save(&old)?;
    let mut windows = registry.restore()?;
    let first = windows.pop().unwrap();
    assert_eq!(first.store.last(), Some(old.clone()));
    let second = registry.create()?;
    let second_id = second.id;
    assert_eq!(second.store.last(), None);
    registry.close(first)?;
    drop(second);
    let mut windows = registry.restore()?;
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].id, second_id);
    registry.close(windows.pop().unwrap())?;
    assert!(registry.restore()?.is_empty());
    assert_eq!(legacy.last(), Some(old));
    Ok(())
}

#[test]
fn preserves_future_window_schemas_and_recovers_corrupt_lists() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let registry = WindowRegistry::new(fixture.path().join("desktop-view"));
    let initial = registry.restore()?;
    let second = registry.create()?;
    let directory = Directory::open(&registry.path)?;
    let bytes = br#"{ "schema_version": 2, "windows": [] }"#;
    directory.replace_private_file(FILE, bytes)?;
    assert!(registry.restore().is_err());
    assert!(registry.create().is_err());
    assert_eq!(
        directory.read_private_file(FILE, MAX_BYTES)?,
        Some(bytes.to_vec())
    );
    directory.replace_private_file(FILE, b"{broken")?;
    drop(initial);
    drop(second);
    assert_eq!(registry.restore()?.len(), 1);
    Ok(())
}

#[test]
fn simultaneous_launches_claim_each_saved_window_once() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let registry = WindowRegistry::new(fixture.path().join("desktop-view"));
    drop(registry.restore()?);
    drop(registry.create()?);
    let barrier = std::sync::Barrier::new(2);
    thread::scope(|scope| {
        let tasks: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    registry.restore().unwrap()
                })
            })
            .collect();
        let windows: Vec<_> = tasks
            .into_iter()
            .flat_map(|task| task.join().unwrap())
            .collect();
        let ids: HashSet<_> = windows.iter().map(|window| window.id).collect();
        assert_eq!(windows.len(), 2);
        assert_eq!(ids.len(), 2);
    });
    Ok(())
}
