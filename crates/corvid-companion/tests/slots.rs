use std::collections::{BTreeMap, BTreeSet};

use corvid_companion::{
    HostEndpoint, Recovery, RunKey, RunObservation, SlotLayout, Store, initialize,
};
use uuid::Uuid;

fn key(run: u128) -> RunKey {
    RunKey {
        host: Uuid::from_u128(1),
        epoch: Uuid::from_u128(1),
        run: Uuid::from_u128(run),
    }
}

fn live(sequence: u64) -> RunObservation {
    RunObservation {
        created_sequence: Some(sequence),
        pane: Some(Uuid::from_u128(u128::from(sequence))),
        navigable: true,
        eligible: true,
        ended: false,
    }
}

fn endpoint() -> HostEndpoint {
    HostEndpoint::Ssh {
        alias: "configured-host".to_owned(),
    }
}

fn private_parent() -> tempfile::TempDir {
    #[cfg(target_os = "linux")]
    return tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    #[cfg(windows)]
    tempfile::tempdir().unwrap()
}

#[test]
fn retains_completed_agents_and_packs_the_queue_after_pane_closure() {
    let mut layout = SlotLayout::new(3).unwrap();
    let [a, b, c, d, e] = [key(10), key(8), key(4), key(5), key(6)];
    let mut runs = BTreeMap::from([
        (a, live(1)),
        (b, live(2)),
        (c, live(3)),
        (d, live(4)),
        (e, live(5)),
    ]);
    let removed = BTreeSet::new();
    layout.reconcile(&runs, &removed).unwrap();
    assert_eq!(layout.slots()[0].run, Some(a));
    assert_eq!(layout.slots()[1].run, Some(b));
    assert_eq!(layout.slots()[2].run, Some(c));
    assert_eq!(layout.overflow().collect::<Vec<_>>(), [d, e]);
    layout
        .pin(3, layout.slots()[2].revision, Some(c), true)
        .unwrap();
    let pressed = layout.slots()[1].clone();
    for run in [b, d] {
        runs.get_mut(&run).unwrap().ended = true;
        runs.get_mut(&run).unwrap().navigable = false;
    }
    layout.reconcile(&runs, &removed).unwrap();
    assert_eq!(layout.slots()[1], pressed);
    assert!(layout.ended().next().is_none());
    assert_eq!(layout.overflow().collect::<Vec<_>>(), [d, e]);

    layout.reconcile(&runs, &BTreeSet::from([b])).unwrap();
    assert_eq!(
        layout
            .slots()
            .iter()
            .filter_map(|slot| slot.run)
            .collect::<Vec<_>>(),
        [a, c, d]
    );
    assert!(layout.slots()[1].pinned);
    assert_eq!(layout.overflow().collect::<Vec<_>>(), [e]);
    assert_eq!(
        layout
            .exact_slot(2, pressed.revision, Some(b))
            .unwrap_err()
            .code,
        "StaleBinding"
    );
    layout.reconcile(&runs, &BTreeSet::from([a, c])).unwrap();
    assert_eq!(
        layout
            .slots()
            .iter()
            .filter_map(|slot| slot.run)
            .collect::<Vec<_>>(),
        [d, e]
    );
    assert!(layout.overflow().next().is_none());
    assert!(layout.slots().iter().all(|slot| !slot.pinned));
}

#[test]
fn admits_an_already_completed_run_with_an_existing_pane() {
    let mut completed = live(1);
    completed.navigable = false;
    completed.ended = true;
    let mut layout = SlotLayout::new(1).unwrap();
    layout
        .reconcile(&BTreeMap::from([(key(2), completed)]), &BTreeSet::new())
        .unwrap();
    assert_eq!(layout.slots()[0].run, Some(key(2)));
}

#[test]
fn retains_missing_and_old_epoch_bindings_until_explicit_evidence() {
    let mut layout = SlotLayout::new(1).unwrap();
    let a = key(2);
    let b = RunKey {
        epoch: Uuid::from_u128(2),
        ..a
    };
    let runs = BTreeMap::from([(a, live(1))]);
    layout.reconcile(&runs, &BTreeSet::new()).unwrap();
    let original = layout.slots()[0].clone();

    let replacement = BTreeMap::from([(b, live(1))]);
    layout.reconcile(&replacement, &BTreeSet::new()).unwrap();
    assert_eq!(layout.slots()[0], original);
    assert_eq!(layout.overflow().collect::<Vec<_>>(), [b]);

    layout
        .reconcile(&replacement, &BTreeSet::from([a]))
        .unwrap();
    assert_eq!(layout.slots()[0].run, Some(b));
    assert_eq!(layout.ended().collect::<Vec<_>>(), [a]);
    assert!(layout.exact_slot(1, original.revision, Some(a)).is_err());
}

#[test]
fn keeps_controller_and_slots_across_restart_and_recovers_from_backup() {
    let parent = private_parent();
    let path = parent.path().join("companion");
    let run = key(2);
    let controller = initialize(&path, run.host, endpoint(), 2).unwrap();
    let mut store = Store::open(&path).unwrap();
    assert!(Store::open(&path).is_err());
    store
        .update(|layout| {
            layout.reconcile(&BTreeMap::from([(run, live(1))]), &BTreeSet::new())?;
            Ok(())
        })
        .unwrap();
    let slot = store.layout().slots()[0].clone();
    store
        .update(|layout| {
            layout.pin(1, slot.revision, Some(run), true)?;
            Ok(())
        })
        .unwrap();
    drop(store);

    let current = Store::open(&path).unwrap();
    assert_eq!(current.config().controller_id, controller);
    assert!(current.layout().slots()[0].pinned);
    drop(current);

    std::fs::write(path.join("slots.json"), b"broken").unwrap();
    let backup = Store::open(&path).unwrap();
    assert_eq!(backup.recovery(), Recovery::Backup);
    assert_eq!(backup.layout().slots()[0], slot);
    assert_eq!(
        std::fs::read(path.join("slots.json.corrupt")).unwrap(),
        b"broken"
    );
}

#[test]
fn refuses_to_replace_newer_state_or_corrupt_identity() {
    let parent = private_parent();
    let path = parent.path().join("companion");
    let host = Uuid::new_v4();
    initialize(&path, host, endpoint(), 2).unwrap();
    drop(Store::open(&path).unwrap());
    let newer = br#"{"schema_version":2}"#;
    std::fs::write(path.join("slots.json"), newer).unwrap();
    assert!(
        Store::open(&path)
            .err()
            .unwrap()
            .to_string()
            .contains("unsupported schema")
    );
    assert_eq!(std::fs::read(path.join("slots.json")).unwrap(), newer);
    std::fs::write(path.join("config.json"), b"broken").unwrap();
    assert!(Store::open(&path).is_err());
    assert!(initialize(&path, host, endpoint(), 2).is_err());
    assert_eq!(std::fs::read(path.join("config.json")).unwrap(), b"broken");
}
