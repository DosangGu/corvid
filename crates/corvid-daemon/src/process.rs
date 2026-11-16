use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use corvid_core::agent::{ProcessLiveness, Provider};
use corvid_protocol::collector::CODEX_VERSION;
use nix::unistd::geteuid;
use uuid::Uuid;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
pub(crate) use linux::boot_id;
#[cfg(target_os = "linux")]
use linux::{ProcessDirectory, read_stat};
#[cfg(target_os = "macos")]
pub(crate) use macos::boot_id;
#[cfg(target_os = "macos")]
use macos::{ProcessDirectory, read_stat};

const PROBE_SPACING: Duration = Duration::from_millis(50);
const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const OUTPUT_DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Default)]
pub(crate) struct ProbeBudget(Mutex<ProbeSchedule>);

#[derive(Default)]
struct ProbeSchedule {
    next: Option<Instant>,
    waiting: VecDeque<Uuid>,
}

impl ProbeBudget {
    fn acquire(&self, terminal: Uuid, now: Instant) -> bool {
        let mut schedule = self.0.lock().expect("probe budget lock poisoned");
        if !schedule.waiting.contains(&terminal) {
            schedule.waiting.push_back(terminal);
        }
        if schedule.waiting.front() != Some(&terminal)
            || schedule.next.is_some_and(|deadline| now < deadline)
        {
            return false;
        }
        schedule.waiting.pop_front();
        schedule.next = Some(now + PROBE_SPACING);
        true
    }

    fn cancel(&self, terminal: Uuid) {
        let mut schedule = self.0.lock().expect("probe budget lock poisoned");
        schedule.waiting.retain(|waiting| *waiting != terminal);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Incarnation {
    pub(crate) boot_id: Uuid,
    pub(crate) pid: u32,
    pub(crate) start_ticks: u64,
    pub(crate) foreground_group: Option<i32>,
}

struct Stat {
    parent_pid: u32,
    process_group: i32,
    tty: i32,
    start_ticks: u64,
    foreground_group: Option<i32>,
    exited: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Executable {
    device: u64,
    inode: u64,
}

impl Executable {
    pub(crate) fn read(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        metadata.is_file().then_some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessImage {
    pub(crate) incarnation: Incarnation,
    pub(crate) executable: Executable,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct HookProducer {
    pub(crate) pid: u32,
    pub(crate) start_ticks: u64,
    pub(crate) executable: Executable,
}

impl ProcessImage {
    pub(crate) fn capture(incarnation: Incarnation, expected: Option<Executable>) -> Option<Self> {
        let process = ProcessDirectory::open(incarnation.pid)?;
        let stat = process.stat()?;
        if stat.exited || stat.start_ticks != incarnation.start_ticks {
            return None;
        }
        let executable = process.executable()?;
        if expected.is_some_and(|expected| expected != executable) {
            return None;
        }
        Some(Self {
            incarnation,
            executable,
        })
    }

    pub(crate) fn capture_started(
        incarnation: Incarnation,
        expected: Option<Executable>,
    ) -> Option<Self> {
        // portable-pty closes the spawn error pipe in pre_exec on Unix, so
        // spawn can return before exec on both Linux and macOS. On Darwin,
        // /bin/sh may then exec the selected system shell as well.
        // Qualified native providers must reach the expected inode. For an
        // unqualified script, require two stable image samples at startup.
        let helper = helper_image()?;
        let deadline = Instant::now() + Duration::from_millis(250);
        let mut candidate = None;
        let mut sampled = Instant::now();
        loop {
            if let Some(image) = Self::capture(incarnation, None)
                && image.executable != helper
            {
                if expected == Some(image.executable) {
                    return Some(image);
                }
                if expected.is_none() {
                    if candidate == Some(image.executable) {
                        if sampled.elapsed() >= Duration::from_millis(20) {
                            return Some(image);
                        }
                    } else {
                        candidate = Some(image.executable);
                        sampled = Instant::now();
                    }
                }
            } else {
                // exec can interrupt a process query. Retry only within this
                // startup bound; capture still checks the original birth time.
                candidate = None;
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    pub(crate) fn is_current(self) -> bool {
        self.current_stat().is_some()
    }

    pub(crate) fn shell_exit(self, pending: bool) -> Option<ProcessLiveness> {
        let pid = self.incarnation.pid;
        let directory = match ProcessDirectory::try_open(pid) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Some(ProcessLiveness::Exited { code: None });
            }
            Err(_) => return Some(ProcessLiveness::Unknown),
        };
        let Some(stat) = directory.stat() else {
            return Some(ProcessLiveness::Unknown);
        };
        if stat.exited || stat.start_ticks != self.incarnation.start_ticks {
            return Some(ProcessLiveness::Exited { code: None });
        }
        let image = directory.executable();
        let starting = pending && image.is_some() && image == helper_image();
        (image != Some(self.executable) && !starting).then_some(ProcessLiveness::Unknown)
    }

    fn current_stat(self) -> Option<Stat> {
        let process = ProcessDirectory::open(self.incarnation.pid)?;
        let stat = process.stat()?;
        (!stat.exited
            && stat.start_ticks == self.incarnation.start_ticks
            && process.executable() == Some(self.executable))
        .then_some(stat)
    }

    /// Scope an observer to the managed launcher or its qualified local broker.
    /// Tool descendants may inherit the launch association without owning it.
    pub(crate) fn verify_peer(self, peer: u32, provider: Provider) -> Option<HookProducer> {
        let pid = self.incarnation.pid;
        let launcher = self.current_stat()?;
        let observer = read_stat(peer)?;
        if observer.exited {
            return None;
        }
        if observer.parent_pid == pid {
            return Some(HookProducer {
                pid,
                start_ticks: launcher.start_ticks,
                executable: self.executable,
            });
        }
        if provider != Provider::Codex {
            return None;
        }
        codex_broker(observer.parent_pid, pid)
    }
}

pub(crate) fn verify_shell_launcher(shell: Incarnation, peer: u32) -> Option<Incarnation> {
    let root = read_stat(shell.pid)?;
    if root.exited || root.start_ticks != shell.start_ticks || root.tty == 0 {
        return None;
    }
    let process = ProcessDirectory::open(peer)?;
    let stat = process.stat()?;
    if stat.exited
        || (peer != shell.pid && stat.parent_pid != shell.pid)
        || (peer == shell.pid && stat.start_ticks != shell.start_ticks)
        || stat.tty != root.tty
        || stat.foreground_group != Some(stat.process_group)
        || process.executable()? != helper_image()?
    {
        return None;
    }
    Some(stat.incarnation(peer, shell.boot_id))
}

fn helper_image() -> Option<Executable> {
    Executable::read(&std::env::current_exe().ok()?)
}

fn codex_broker(pid: u32, launcher: u32) -> Option<HookProducer> {
    let stat = read_stat(pid)?;
    if stat.exited || stat.parent_pid != launcher {
        return None;
    }
    let process = ProcessDirectory::open(pid)?;
    let image = process.path()?;
    let installed = image.clone();
    let target = if cfg!(target_os = "macos") {
        "aarch64-apple-darwin"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64-unknown-linux-musl"
    } else {
        "x86_64-unknown-linux-musl"
    };
    let release = format!("packages/app-server-daemon/releases/{CODEX_VERSION}-{target}/bin/codex");
    if !installed.ends_with(release) {
        return None;
    }
    let metadata = image.metadata().ok()?;
    if metadata.uid() != geteuid().as_raw() || !metadata.is_file() || metadata.mode() & 0o022 != 0 {
        return None;
    }
    let args = process.arguments()?;
    if args.len() > 4096 || !codex_broker_args(&args) {
        return None;
    }
    let executable = process.executable()?;
    if executable
        != (Executable {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    {
        return None;
    }
    let after = read_stat(pid)?;
    if after.exited
        || after.start_ticks != stat.start_ticks
        || after.parent_pid != launcher
        || Executable::read(&image) != Some(executable)
    {
        return None;
    }
    Some(HookProducer {
        pid,
        start_ticks: stat.start_ticks,
        executable,
    })
}

fn codex_broker_args(args: &[u8]) -> bool {
    let mut argv = args
        .strip_suffix(b"\0")
        .unwrap_or(args)
        .split(|byte| *byte == 0);
    if argv.next().is_none_or(|program| program.is_empty()) {
        return false;
    }
    let rest: Vec<_> = argv.collect();
    rest == [
        b"app-server".as_slice(),
        b"--listen",
        b"unix://",
        b"--analytics-default-enabled",
        b"--managed-daemon",
    ] || rest
        == [
            b"app-server".as_slice(),
            b"--listen",
            b"unix://",
            b"--managed-daemon",
        ]
}

pub(crate) fn incarnation(pid: u32, boot_id: Uuid) -> Option<Incarnation> {
    let stat = read_stat(pid)?;
    (!stat.exited).then(|| stat.incarnation(pid, boot_id))
}

impl Stat {
    fn incarnation(&self, pid: u32, boot_id: Uuid) -> Incarnation {
        Incarnation {
            boot_id,
            pid,
            start_ticks: self.start_ticks,
            foreground_group: self.foreground_group,
        }
    }
}

pub(crate) struct Probe {
    budget: Arc<ProbeBudget>,
    terminal: Uuid,
    pid: u32,
    boot_id: Uuid,
    next: Instant,
    output_after: Instant,
    identity: Option<Incarnation>,
    executable: Option<Executable>,
    cwd: Option<PathBuf>,
}

pub(crate) enum ProbeResult {
    Status(ProcessLiveness, Option<Incarnation>),
    ImageChanged(Incarnation),
}

impl Probe {
    pub(crate) fn new(
        budget: Arc<ProbeBudget>,
        pid: u32,
        boot_id: Uuid,
        terminal: Uuid,
        identity: Option<Incarnation>,
        executable: Option<Executable>,
    ) -> Self {
        let stagger = Duration::from_millis((terminal.as_u128() % 2_000) as u64);
        let now = Instant::now();
        Self {
            budget,
            terminal,
            pid,
            boot_id,
            next: now + stagger,
            output_after: now + OUTPUT_DEBOUNCE,
            identity,
            executable,
            cwd: None,
        }
    }

    pub(crate) fn sample(&mut self, output: bool) -> Option<(ProbeResult, Option<PathBuf>)> {
        let now = Instant::now();
        if output && now >= self.output_after {
            self.next = self.next.min(now);
        }
        if now < self.next || !self.budget.acquire(self.terminal, now) {
            return None;
        }
        self.next = now + SAMPLE_INTERVAL;
        self.output_after = now + OUTPUT_DEBOUNCE;
        Some(self.read())
    }

    fn read(&mut self) -> (ProbeResult, Option<PathBuf>) {
        let unknown = (ProbeResult::Status(ProcessLiveness::Unknown, None), None);
        let Some(process) = ProcessDirectory::open(self.pid) else {
            return unknown;
        };
        let Some(stat) = process.stat() else {
            return unknown;
        };
        if self
            .identity
            .is_some_and(|identity| identity.start_ticks != stat.start_ticks)
        {
            return unknown;
        }
        let identity = stat.incarnation(self.pid, self.boot_id);
        self.identity = Some(identity);
        if stat.exited {
            return (
                ProbeResult::Status(ProcessLiveness::Exited { code: None }, Some(identity)),
                None,
            );
        }
        let cwd = if stat.parent_pid == std::process::id() {
            self.changed_cwd(&process)
        } else {
            None
        };
        if let Some(expected) = self.executable {
            let Some(executable) = process.executable() else {
                return (unknown.0, cwd);
            };
            if executable != expected {
                self.executable = None;
                return (ProbeResult::ImageChanged(identity), cwd);
            }
        }
        (
            ProbeResult::Status(ProcessLiveness::Running, Some(identity)),
            cwd,
        )
    }

    fn changed_cwd(&mut self, process: &ProcessDirectory) -> Option<PathBuf> {
        let path = process.cwd()?;
        if !path.is_absolute() || self.cwd.as_ref() == Some(&path) {
            return None;
        }
        self.cwd = Some(path.clone());
        Some(path)
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.budget.cancel(self.terminal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use linux::parse_stat;
    #[cfg(target_os = "linux")]
    use nix::fcntl::readlinkat;

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
        assert_eq!(stat.start_ticks, 987);
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
                start_ticks: current.start_ticks + 1,
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

        let executable = Executable::read(Path::new("/bin/sleep")).unwrap();
        // TODO: Give the process startup fixture distinct executable images on multicall coreutils
        // Ubuntu installations using multicall coreutils can give /bin/sleep and /bin/cat the same device and inode, invalidating the negative case. Use a fixture executable with a distinct image instead of assuming these commands differ.
        let different = Executable::read(Path::new("/bin/cat")).unwrap();
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
                .env("CORVID_TEST_DELAYED_EXEC", "1")
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
                    == helper_image().unwrap()
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
                assert_eq!(image.incarnation.start_ticks, identity.start_ticks);
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

        if std::env::var_os("CORVID_TEST_DELAYED_EXEC").is_none() {
            return;
        }
        println!("CORVID_EXEC_READY");
        std::io::stdout().flush().unwrap();
        std::io::stdin().read_exact(&mut [0]).unwrap();
        let error = std::process::Command::new("/bin/sleep").arg("1").exec();
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
        assert_eq!(probe.read().1.as_deref(), Some(original.as_path()));

        fs::remove_dir(&original).unwrap();
        fs::create_dir(&decoy).unwrap();
        let process = ProcessDirectory::open(child.id()).unwrap();
        assert_eq!(
            readlinkat(&process.directory, "cwd").unwrap(),
            decoy.as_os_str()
        );
        assert!(probe.read().1.is_none());

        drop(child.stdin.take());
        child.wait().unwrap();
    }
}
