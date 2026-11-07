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
fn orders_by_creation_then_preserves_pinned_slots_and_overflow() {
    let mut layout = SlotLayout::new(2).unwrap();
    let [a, b, c, d] = [key(10), key(8), key(4), key(5)];
    let mut runs = BTreeMap::from([(a, live(1)), (b, live(2)), (c, live(3))]);
    let removed = BTreeSet::new();
    layout.reconcile(&runs, &removed).unwrap();
    let revision = layout.slots()[0].revision;
    assert_eq!(layout.slots()[0].run, Some(a));
    assert_eq!(layout.slots()[1].run, Some(b));
    assert_eq!(layout.overflow().collect::<Vec<_>>(), [c]);

    layout.pin(1, revision, Some(a), true).unwrap();
    let pressed = layout.slots()[0].revision;
    runs.insert(d, live(4));
    runs.get_mut(&a).unwrap().ended = true;
    layout.reconcile(&runs, &removed).unwrap();
    assert!(layout.slots()[0].pinned);
    assert_eq!(layout.slots()[0].run, None);
    assert_eq!(layout.ended().collect::<Vec<_>>(), [a]);
    assert_eq!(layout.overflow().collect::<Vec<_>>(), [c, d]);
    assert_eq!(
        layout.exact_slot(1, pressed, Some(a)).unwrap_err().code,
        "StaleBinding"
    );

    layout.place(1, layout.slots()[0].revision, d).unwrap();
    layout.remove(b).unwrap();
    layout.reconcile(&runs, &removed).unwrap();
    assert_eq!(layout.slots()[0].run, Some(d));
    assert_eq!(layout.slots()[1].run, Some(c));
    assert_eq!(layout.suppressed().collect::<Vec<_>>(), [b]);
    layout.readd(b).unwrap();
    layout.reconcile(&runs, &removed).unwrap();
    assert_eq!(layout.overflow().collect::<Vec<_>>(), [b]);
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
