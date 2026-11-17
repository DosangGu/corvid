use super::*;
#[cfg(target_os = "linux")]
use native::parse_stat;
#[cfg(target_os = "linux")]
use nix::fcntl::readlinkat;

#[test]
fn recognizes_codex_broker_releases_without_a_version_pin() {
    for target in [
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "aarch64-apple-darwin",
    ] {
        for version in ["0.166.0", "0.168.0", "0.168.0-alpha.1", "1.0.0"] {
            let path = PathBuf::from(format!(
                "/private/packages/app-server-daemon/releases/{version}-{target}/bin/codex"
            ));
            assert!(codex_broker_path(&path, target));
            assert!(!codex_broker_path(&path, "different-target"));
        }
    }
}

#[test]
fn rejects_unrecognized_broker_layouts_and_commands() {
    let target = "x86_64-unknown-linux-musl";
    for path in [
        format!("/private/releases/0.168.0-{target}/bin/codex"),
        format!("/private/packages/other/releases/0.168.0-{target}/bin/codex"),
        format!("/private/packages/app-server-daemon/releases/latest-{target}/bin/codex"),
        format!("/private/packages/app-server-daemon/releases/0.168.0-{target}/bin/other"),
    ] {
        assert!(!codex_broker_path(Path::new(&path), target), "{path}");
    }
    assert!(codex_broker_args(
        b"codex\0app-server\0--listen\0unix://\0--managed-daemon\0"
    ));
    assert!(!codex_broker_args(
        b"codex\0app-server\0--listen\0tcp://127.0.0.1:1234\0--managed-daemon\0"
    ));
    assert!(!codex_broker_args(
        b"codex\0app-server\0--listen\0unix://\0"
    ));
}

#[test]
fn waiting_probes_cannot_be_overtaken_and_cancelled_probes_release_the_queue() {
    let budget = ProbeBudget::default();
    let noisy = Uuid::new_v4();
    let silent = Uuid::new_v4();
    let stopped = Uuid::new_v4();
    let start = Instant::now();
    assert!(budget.acquire(noisy, start));
    assert!(!budget.acquire(stopped, start));
    assert!(!budget.acquire(silent, start));
    assert!(!budget.acquire(noisy, start));

    budget.cancel(stopped);
    let next = start + PROBE_SPACING;
    assert!(!budget.acquire(noisy, next));
    assert!(budget.acquire(silent, next));
    assert!(!budget.acquire(noisy, next));
    assert!(budget.acquire(noisy, next + PROBE_SPACING));
}

#[cfg(target_os = "linux")]
#[test]
fn parses_foreground_group_and_start_time_after_arbitrary_command_bytes() {
    let stat = parse_stat(
        b"31 (name) \xff x) Z 8 30 30 34816 46 0 0 0 0 0 0 0 0 0 0 0 0 0 987",
        31,
    )
    .unwrap();
    assert_eq!(stat.parent_pid, 8);
    assert_eq!(stat.birth, BirthIdentity::from_raw(987));
    assert_eq!(stat.foreground_group, Some(46));
    assert!(stat.exited);
}

#[cfg(target_os = "linux")]
#[test]
fn rejects_mismatched_pid_and_missing_start_time() {
    assert!(parse_stat(b"32 (name) R 1 2 3 4 5", 31).is_none());
    assert!(parse_stat(b"31 (name) R 1 2 3 4 5", 31).is_none());
}

#[test]
fn rejects_different_start_time_for_the_same_pid_and_executable() {
    let current = incarnation(std::process::id(), boot_id().unwrap()).unwrap();
    let image = ProcessImage::capture(current, None).unwrap();
    assert!(image.is_current());

    let stale = ProcessImage {
        incarnation: Incarnation {
            birth: BirthIdentity::from_raw(current.birth.wire_value().wrapping_add(1)),
            ..current
        },
        ..image
    };
    assert!(!stale.is_current());
    assert!(ProcessImage::capture(stale.incarnation, Some(image.executable)).is_none());
}

#[test]
fn startup_capture_waits_for_exec_before_binding_process_image() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    // Copies have distinct inodes even on multicall coreutils installations.
    let fixture = tempfile::tempdir().unwrap();
    let program = fixture.path().join("expected/sleep");
    let other = fixture.path().join("other/sleep");
    fs::create_dir_all(program.parent().unwrap()).unwrap();
    fs::create_dir_all(other.parent().unwrap()).unwrap();
    fs::copy("/bin/sleep", &program).unwrap();
    fs::copy("/bin/sleep", &other).unwrap();
    let executable = Executable::read(&program).unwrap();
    let different = Executable::read(&other).unwrap();
    assert_ne!(executable, different);
    for (expected, should_bind) in [
        (Some(executable), true),
        (None, true),
        (Some(different), false),
    ] {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process::tests::delayed_exec_child",
                "--nocapture",
            ])
            .env("CORVID_TEST_DELAYED_EXEC", &program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        loop {
            let mut line = String::new();
            assert!(
                output.read_line(&mut line).unwrap() > 0,
                "child exited before ready"
            );
            if line.trim_end().ends_with("CORVID_EXEC_READY") {
                break;
            }
        }
        let identity = incarnation(child.id(), boot_id().unwrap()).unwrap();
        assert!(
            ProcessImage::capture(identity, None).unwrap().executable
                == NativeInspector.helper_image().unwrap()
        );
        let mut input = child.stdin.take().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            input.write_all(b"x").unwrap();
        });

        let captured = ProcessImage::capture_started(identity, expected);
        release.join().unwrap();
        let _ = child.kill();
        child.wait().unwrap();
        if should_bind {
            let image = captured.expect("startup must wait for the launched image");
            assert!(image.executable == executable);
            assert_eq!(image.incarnation.birth, identity.birth);
        } else {
            assert!(
                captured.is_none(),
                "a different executable must not be bound"
            );
        }
    }
}

#[test]
fn delayed_exec_child() {
    use std::io::{Read, Write};
    use std::os::unix::process::CommandExt;

    let Some(program) = std::env::var_os("CORVID_TEST_DELAYED_EXEC") else {
        return;
    };
    println!("CORVID_EXEC_READY");
    std::io::stdout().flush().unwrap();
    std::io::stdin().read_exact(&mut [0]).unwrap();
    let error = std::process::Command::new(program)
        .arg0("sleep")
        .arg("1")
        .exec();
    panic!("fixture exec failed: {error}");
}

#[cfg(target_os = "linux")]
#[test]
fn rejects_a_deleted_directory_even_if_the_rendered_path_exists() {
    use std::process::{Command, Stdio};

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let original = root.join("working");
    let decoy = root.join("working (deleted)");
    fs::create_dir(&original).unwrap();
    let mut child = Command::new("/bin/sh")
        .args(["-c", "IFS= read -r line"])
        .current_dir(&original)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let boot = boot_id().unwrap();
    let identity = incarnation(child.id(), boot).unwrap();
    let mut probe = Probe::new(
        Arc::new(ProbeBudget::default()),
        child.id(),
        boot,
        Uuid::new_v4(),
        Some(identity),
        None,
    );
    assert_eq!(
        probe.read_with(&NativeInspector).1.as_deref(),
        Some(original.as_path())
    );

    fs::remove_dir(&original).unwrap();
    fs::create_dir(&decoy).unwrap();
    let process = NativeInspector.open(child.id()).unwrap();
    assert_eq!(
        readlinkat(&process.directory, "cwd").unwrap(),
        decoy.as_os_str()
    );
    assert!(probe.read_with(&NativeInspector).1.is_none());

    drop(child.stdin.take());
    child.wait().unwrap();
}

struct ObservedChild(std::process::Child);

impl Drop for ObservedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn observation_child(directory: &Path, extra_argument: &str) -> ObservedChild {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let mut child = ObservedChild(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process::tests::process_observation_child",
                "--nocapture",
                "--skip",
            ])
            .arg(extra_argument)
            .env("CORVID_TEST_PROCESS_OBSERVATION", "1")
            .current_dir(directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert!(
            output.read_line(&mut line).unwrap() > 0,
            "observation child exited"
        );
        if line.trim_end().ends_with("CORVID_OBSERVATION_READY") {
            return child;
        }
    }
}

#[test]
fn process_observation_child() {
    use std::io::{Read, Write};
    if std::env::var_os("CORVID_TEST_PROCESS_OBSERVATION").is_none() {
        return;
    }
    println!("CORVID_OBSERVATION_READY");
    std::io::stdout().flush().unwrap();
    let _ = std::io::stdin().read_exact(&mut [0]);
}

#[test]
fn native_handle_observes_one_birth_and_rejects_queries_after_reaping() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().canonicalize().unwrap();
    let mut child = observation_child(&directory, "unused-filter");
    let handle = NativeInspector.open(child.0.id()).unwrap();
    let stat = handle.stat().unwrap();
    assert_eq!(
        corvid_protocol::unix::process_image_path(child.0.id()).unwrap(),
        handle.path().unwrap()
    );
    assert!(!stat.exited);
    assert_eq!(stat.parent_pid, std::process::id());
    assert_eq!(handle.executable().ok(), NativeInspector.helper_image());
    assert_eq!(
        Executable::read(&handle.path().unwrap()),
        NativeInspector.helper_image()
    );
    assert_eq!(handle.cwd().unwrap(), directory);
    assert!(
        handle
            .arguments()
            .unwrap()
            .windows(b"unused-filter".len())
            .any(|part| part == b"unused-filter")
    );
    assert_eq!(handle.stat().unwrap().birth, stat.birth);
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert!(handle.stat().is_err());
    assert!(handle.executable().is_err());
    assert!(handle.cwd().is_err());
    assert!(matches!(
        NativeInspector.open(child.0.id()),
        Err(ObservationError::Gone)
    ));
}

#[test]
fn native_arguments_reject_oversized_commands_without_returning_a_prefix() {
    let temporary = tempfile::tempdir().unwrap();
    let child = observation_child(
        temporary.path(),
        &"a".repeat(inspection::ARGUMENT_LIMIT + 1),
    );
    let handle = NativeInspector.open(child.0.id()).unwrap();
    assert_eq!(handle.arguments(), Err(ObservationError::TooLarge));
}

#[test]
fn native_cwd_rejects_a_replacement_directory_at_the_same_path() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().canonicalize().unwrap().join("working");
    fs::create_dir(&path).unwrap();
    let child = observation_child(&path, "unused-filter");
    let handle = NativeInspector.open(child.0.id()).unwrap();
    assert_eq!(handle.cwd().unwrap(), path);
    fs::remove_dir(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(handle.cwd().is_err());
}
