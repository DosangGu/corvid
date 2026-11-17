//! Deterministic policy tests: no procfs/libproc, child scheduling, or sleeps.
use std::cell::{Cell, RefCell};

use super::inspection::{ARGUMENT_LIMIT, Observation};
use super::*;

const PID: u32 = 101;
const HELPER: Executable = Executable {
    device: 1,
    inode: 1,
};
const AGENT: Executable = Executable {
    device: 1,
    inode: 2,
};
const OTHER: Executable = Executable {
    device: 1,
    inode: 3,
};

#[derive(Clone)]
struct Sample {
    stats: RefCell<VecDeque<Observation<Stat>>>,
    executable: Observation<Executable>,
    path: Observation<PathBuf>,
    arguments: Observation<Vec<u8>>,
    cwd: Observation<PathBuf>,
}

impl Sample {
    fn running(executable: Executable) -> Self {
        Self {
            stats: RefCell::new(VecDeque::from([Ok(stat())])),
            executable: Ok(executable),
            path: Err(ObservationError::Unavailable),
            arguments: Err(ObservationError::Unavailable),
            cwd: Err(ObservationError::Unavailable),
        }
    }

    fn with_stat(self, stat: Observation<Stat>) -> Self {
        *self.stats.borrow_mut() = VecDeque::from([stat]);
        self
    }
}

fn next<T: Clone>(queue: &RefCell<VecDeque<T>>) -> T {
    let mut queue = queue.borrow_mut();
    if queue.len() > 1 {
        queue.pop_front().unwrap()
    } else {
        queue
            .front()
            .expect("script must have a final observation")
            .clone()
    }
}

impl ProcessObservation for Sample {
    fn stat(&self) -> Observation<Stat> {
        next(&self.stats)
    }
    fn executable(&self) -> Observation<Executable> {
        self.executable
    }
    fn path(&self) -> Observation<PathBuf> {
        self.path.clone()
    }
    fn arguments(&self) -> Observation<Vec<u8>> {
        self.arguments.clone()
    }
    fn cwd(&self) -> Observation<PathBuf> {
        self.cwd.clone()
    }
}

struct Script {
    steps: RefCell<VecDeque<(u32, Observation<Sample>)>>,
    opens: Cell<usize>,
}

impl Script {
    fn new(steps: impl IntoIterator<Item = (u32, Observation<Sample>)>) -> Self {
        Self {
            steps: RefCell::new(steps.into_iter().collect()),
            opens: Cell::new(0),
        }
    }

    fn samples(samples: impl IntoIterator<Item = Observation<Sample>>) -> Self {
        Self::new(samples.into_iter().map(|sample| (PID, sample)))
    }
}

impl ProcessInspector for Script {
    type Handle = Sample;

    fn open(&self, pid: u32) -> Observation<Self::Handle> {
        self.opens.set(self.opens.get() + 1);
        let (expected, result) = next(&self.steps);
        assert_eq!(pid, expected, "policy queried an unexpected process");
        result
    }

    fn helper_image(&self) -> Option<Executable> {
        Some(HELPER)
    }
}

struct FakeClock(Cell<Instant>);

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.0.get()
    }
    fn sleep(&self, duration: Duration) {
        self.0.set(self.now() + duration);
    }
}

fn stat() -> Stat {
    Stat {
        parent_pid: std::process::id(),
        process_group: 10,
        tty: 1,
        birth: BirthIdentity::from_raw(7),
        foreground_group: Some(10),
        exited: false,
    }
}

fn identity() -> Incarnation {
    stat().incarnation(PID, Uuid::nil())
}

fn owner() -> ProcessImage {
    ProcessImage {
        incarnation: identity(),
        executable: AGENT,
    }
}

fn probe() -> Probe {
    Probe::new(
        Arc::new(ProbeBudget::default()),
        PID,
        Uuid::nil(),
        Uuid::nil(),
        Some(identity()),
        Some(AGENT),
    )
}

#[test]
fn rejects_reused_pid_and_changed_or_unavailable_executable() {
    let reused = Stat {
        birth: BirthIdentity::from_raw(8),
        ..stat()
    };
    for sample in [
        Sample::running(AGENT).with_stat(Ok(reused)),
        Sample::running(OTHER),
        Sample {
            executable: Err(ObservationError::Changed),
            ..Sample::running(AGENT)
        },
        Sample::running(AGENT).with_stat(Err(ObservationError::Unavailable)),
    ] {
        let inspector = Script::samples([Ok(sample)]);
        assert!(ProcessImage::capture_with(&inspector, identity(), Some(AGENT)).is_none());
        assert!(owner().current_stat_with(&inspector).is_none());
    }
}

#[test]
fn startup_retries_helper_and_transient_samples_then_binds_expected_image() {
    let inspector = Script::samples([
        Ok(Sample::running(HELPER)),
        Err(ObservationError::Unavailable),
        Ok(Sample::running(AGENT)),
    ]);
    let start = Instant::now();
    let clock = FakeClock(Cell::new(start));
    assert_eq!(
        ProcessImage::capture_started_with(&inspector, &clock, identity(), Some(AGENT)),
        Some(owner())
    );
    assert_eq!(clock.now() - start, Duration::from_millis(4));
}

#[test]
fn startup_deadline_rejects_wrong_images_reused_pids_and_unavailable_processes() {
    for sample in [
        Ok(Sample::running(HELPER)),
        Ok(Sample::running(OTHER)),
        Err(ObservationError::Unavailable),
        Err(ObservationError::Gone),
        Ok(Sample::running(AGENT).with_stat(Ok(Stat {
            birth: BirthIdentity::from_raw(8),
            ..stat()
        }))),
    ] {
        let inspector = Script::samples([sample]);
        let start = Instant::now();
        let clock = FakeClock(Cell::new(start));
        assert!(
            ProcessImage::capture_started_with(&inspector, &clock, identity(), Some(AGENT))
                .is_none()
        );
        assert_eq!(clock.now() - start, Duration::from_millis(250));
        assert_eq!(inspector.opens.get(), 125);
    }
}

#[test]
fn unqualified_startup_requires_stability_after_the_last_failed_sample() {
    let samples = std::iter::once(Ok(Sample::running(HELPER)))
        .chain(std::iter::repeat_n(Ok(Sample::running(AGENT)), 5))
        .chain([
            Err(ObservationError::Unavailable),
            Ok(Sample::running(AGENT)),
        ]);
    let inspector = Script::samples(samples);
    let start = Instant::now();
    let clock = FakeClock(Cell::new(start));
    assert_eq!(
        ProcessImage::capture_started_with(&inspector, &clock, identity(), None),
        Some(owner())
    );
    assert_eq!(clock.now() - start, Duration::from_millis(34));
}

#[test]
fn shell_exit_requires_process_absence_or_birth_exit_evidence() {
    let exited = ProcessLiveness::Exited { code: None };
    for (sample, expected) in [
        (Err(ObservationError::Gone), Some(exited)),
        (
            Err(ObservationError::Unavailable),
            Some(ProcessLiveness::Unknown),
        ),
        (
            Ok(Sample::running(AGENT).with_stat(Err(ObservationError::Gone))),
            Some(ProcessLiveness::Unknown),
        ),
        (
            Ok(Sample::running(AGENT).with_stat(Ok(Stat {
                exited: true,
                ..stat()
            }))),
            Some(exited),
        ),
        (
            Ok(Sample::running(AGENT).with_stat(Ok(Stat {
                birth: BirthIdentity::from_raw(8),
                ..stat()
            }))),
            Some(exited),
        ),
        (Ok(Sample::running(OTHER)), Some(ProcessLiveness::Unknown)),
        (
            Ok(Sample {
                executable: Err(ObservationError::Gone),
                ..Sample::running(AGENT)
            }),
            Some(ProcessLiveness::Unknown),
        ),
        (Ok(Sample::running(AGENT)), None),
    ] {
        assert_eq!(
            owner().shell_exit_with(&Script::samples([sample]), false),
            expected
        );
    }
    let helper = Script::samples([Ok(Sample::running(HELPER))]);
    assert_eq!(owner().shell_exit_with(&helper, true), None);
    assert_eq!(
        owner().shell_exit_with(&helper, false),
        Some(ProcessLiveness::Unknown)
    );
}

#[test]
fn probe_preserves_identity_through_unknown_and_reports_exit_only_with_evidence() {
    let inspector = Script::samples([
        Err(ObservationError::Gone),
        Ok(Sample::running(AGENT).with_stat(Ok(Stat {
            birth: BirthIdentity::from_raw(8),
            ..stat()
        }))),
        Ok(Sample::running(AGENT).with_stat(Err(ObservationError::Changed))),
        Ok(Sample::running(AGENT)),
        Ok(Sample::running(AGENT).with_stat(Ok(Stat {
            exited: true,
            ..stat()
        }))),
    ]);
    let mut probe = probe();
    for _ in 0..3 {
        assert_eq!(
            probe.read_with(&inspector),
            (ProbeResult::Status(ProcessLiveness::Unknown, None), None)
        );
        assert_eq!(probe.identity, Some(identity()));
    }
    assert_eq!(
        probe.read_with(&inspector).0,
        ProbeResult::Status(ProcessLiveness::Running, Some(identity()))
    );
    assert_eq!(
        probe.read_with(&inspector).0,
        ProbeResult::Status(ProcessLiveness::Exited { code: None }, Some(identity()))
    );
}

#[test]
fn probe_distinguishes_exec_change_from_foreground_group_change() {
    let foreground = Stat {
        foreground_group: Some(20),
        ..stat()
    };
    let inspector = Script::samples([
        Ok(Sample::running(AGENT).with_stat(Ok(foreground))),
        Ok(Sample::running(OTHER).with_stat(Ok(foreground))),
    ]);
    let mut probe = probe();
    let identity = foreground.incarnation(PID, Uuid::nil());
    assert_eq!(
        probe.read_with(&inspector).0,
        ProbeResult::Status(ProcessLiveness::Running, Some(identity))
    );
    assert_eq!(
        probe.read_with(&inspector).0,
        ProbeResult::ImageChanged(identity)
    );
    assert_eq!(
        probe.read_with(&inspector).0,
        ProbeResult::Status(ProcessLiveness::Running, Some(identity))
    );
}

#[test]
fn cwd_rejects_invalid_identity_relative_paths_and_non_child_observations() {
    let valid = PathBuf::from("/verified");
    let inspector = Script::samples([
        Ok(Sample {
            cwd: Ok(valid.clone()),
            ..Sample::running(AGENT)
        }),
        Ok(Sample {
            cwd: Err(ObservationError::Invalid),
            ..Sample::running(AGENT)
        }),
        Ok(Sample {
            cwd: Ok(PathBuf::from("relative")),
            ..Sample::running(AGENT)
        }),
        Ok(Sample {
            cwd: Ok(PathBuf::from("/unrelated")),
            ..Sample::running(AGENT).with_stat(Ok(Stat {
                parent_pid: 0,
                ..stat()
            }))
        }),
        Ok(Sample {
            cwd: Ok(valid.clone()),
            ..Sample::running(AGENT)
        }),
    ]);
    let mut probe = probe();
    assert_eq!(probe.read_with(&inspector).1, Some(valid.clone()));
    for _ in 0..4 {
        assert_eq!(probe.read_with(&inspector).1, None);
        assert_eq!(probe.cwd, Some(valid.clone()));
    }
}

#[test]
fn probe_interval_and_output_debounce_do_not_query_before_their_deadlines() {
    let inspector = Script::samples([Ok(Sample::running(AGENT))]);
    let mut probe = probe();
    let start = Instant::now();
    probe.next = start;
    probe.output_after = start;
    assert!(probe.sample_with(&inspector, false, start).is_some());
    assert!(
        probe
            .sample_with(&inspector, true, start + Duration::from_millis(499))
            .is_none()
    );
    assert!(
        probe
            .sample_with(&inspector, true, start + Duration::from_millis(500))
            .is_some()
    );
    assert!(
        probe
            .sample_with(&inspector, false, start + Duration::from_millis(2499))
            .is_none()
    );
    assert!(
        probe
            .sample_with(&inspector, false, start + Duration::from_millis(2500))
            .is_some()
    );
    assert_eq!(inspector.opens.get(), 3);
}

#[test]
fn hook_peer_verification_requires_current_owner_and_direct_parent() {
    let child = Sample::running(HELPER).with_stat(Ok(Stat {
        parent_pid: PID,
        ..stat()
    }));
    let inspector = Script::new([(PID, Ok(Sample::running(AGENT))), (102, Ok(child))]);
    assert_eq!(
        owner().verify_peer_with(&inspector, 102, Provider::Claude),
        Some(HookProducer {
            pid: PID,
            birth: identity().birth,
            executable: AGENT
        })
    );
    let inspector = Script::new([
        (PID, Ok(Sample::running(AGENT))),
        (102, Ok(Sample::running(HELPER))),
    ]);
    assert!(
        owner()
            .verify_peer_with(&inspector, 102, Provider::Claude)
            .is_none()
    );
    let inspector = Script::samples([Ok(Sample::running(OTHER))]);
    assert!(
        owner()
            .verify_peer_with(&inspector, 102, Provider::Claude)
            .is_none()
    );
    assert_eq!(inspector.opens.get(), 1);
}

#[test]
fn shell_launcher_rejects_background_groups_tty_changes_and_reused_roots() {
    for (root, peer, accepted) in [
        (
            stat(),
            Stat {
                parent_pid: PID,
                ..stat()
            },
            true,
        ),
        (
            stat(),
            Stat {
                parent_pid: PID,
                foreground_group: Some(20),
                ..stat()
            },
            false,
        ),
        (
            stat(),
            Stat {
                parent_pid: PID,
                tty: 2,
                ..stat()
            },
            false,
        ),
        (
            Stat {
                birth: BirthIdentity::from_raw(8),
                ..stat()
            },
            stat(),
            false,
        ),
    ] {
        let inspector = Script::new([
            (PID, Ok(Sample::running(AGENT).with_stat(Ok(root)))),
            (102, Ok(Sample::running(HELPER).with_stat(Ok(peer)))),
        ]);
        assert_eq!(
            verify_shell_launcher_with(&inspector, identity(), 102).is_some(),
            accepted
        );
    }
}

#[test]
fn broker_rejects_mixed_identity_samples_and_incomplete_arguments() {
    use std::os::unix::fs::PermissionsExt;
    let target = if cfg!(target_os = "macos") {
        "aarch64-apple-darwin"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64-unknown-linux-musl"
    } else {
        "x86_64-unknown-linux-musl"
    };
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join(format!(
        "packages/app-server-daemon/releases/1.0.0-{target}/bin/codex"
    ));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"synthetic executable identity").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    let executable = Executable::read(&path).unwrap();
    let base = Sample {
        path: Ok(path),
        arguments: Ok(b"codex\0app-server\0--listen\0unix://\0--managed-daemon\0".to_vec()),
        ..Sample::running(executable).with_stat(Ok(Stat {
            parent_pid: PID,
            ..stat()
        }))
    };
    assert!(codex_broker(&Script::new([(102, Ok(base.clone()))]), 102, PID).is_some());
    for sample in [
        Sample {
            arguments: Err(ObservationError::TooLarge),
            ..base.clone()
        },
        Sample {
            arguments: Ok(vec![b'a'; ARGUMENT_LIMIT + 1]),
            ..base.clone()
        },
        Sample {
            arguments: Ok(b"codex\0app-server\0".to_vec()),
            ..base.clone()
        },
        Sample {
            executable: Ok(OTHER),
            ..base.clone()
        },
        Sample {
            stats: RefCell::new(VecDeque::from([
                Ok(Stat {
                    parent_pid: PID,
                    ..stat()
                }),
                Err(ObservationError::Changed),
            ])),
            ..base.clone()
        },
        Sample {
            stats: RefCell::new(VecDeque::from([
                Ok(Stat {
                    parent_pid: PID,
                    ..stat()
                }),
                Ok(Stat {
                    parent_pid: PID,
                    birth: BirthIdentity::from_raw(8),
                    ..stat()
                }),
            ])),
            ..base
        },
    ] {
        assert!(codex_broker(&Script::new([(102, Ok(sample))]), 102, PID).is_none());
    }
}
