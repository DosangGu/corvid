use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use corvid_core::agent::{ProcessLiveness, Provider};
use corvid_protocol::collector::CODEX_VERSION;
use nix::fcntl::{AtFlags, OFlag, openat, readlinkat};
use nix::sys::stat::{Mode, SFlag, fstatat};
use nix::unistd::geteuid;
use uuid::Uuid;

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

    pub(crate) fn is_current(self) -> bool {
        self.current_stat().is_some()
    }

    pub(crate) fn shell_exit(self, pending: bool) -> Option<ProcessLiveness> {
        let pid = self.incarnation.pid;
        let directory = match File::open(format!("/proc/{pid}")) {
            Ok(directory) => ProcessDirectory { pid, directory },
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
    Executable::read(Path::new("/proc/self/exe"))
}

fn codex_broker(pid: u32, launcher: u32) -> Option<HookProducer> {
    let stat = read_stat(pid)?;
    if stat.exited || stat.parent_pid != launcher {
        return None;
    }
    let directory = PathBuf::from(format!("/proc/{pid}"));
    let image = directory.join("exe");
    let installed = fs::read_link(&image).ok()?;
    let release = format!(
        "packages/app-server-daemon/releases/{CODEX_VERSION}-x86_64-unknown-linux-musl/bin/codex"
    );
    if !installed.ends_with(release) {
        return None;
    }
    let metadata = image.metadata().ok()?;
    if metadata.uid() != geteuid().as_raw() || !metadata.is_file() || metadata.mode() & 0o022 != 0 {
        return None;
    }
    let mut args = Vec::new();
    File::open(directory.join("cmdline"))
        .ok()?
        .take(4097)
        .read_to_end(&mut args)
        .ok()?;
    if args.len() > 4096 || !codex_broker_args(&args) {
        return None;
    }
    let executable = Executable {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
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

pub(crate) fn boot_id() -> Option<Uuid> {
    let value = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Uuid::parse_str(value.trim()).ok()
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
        let path = PathBuf::from(readlinkat(&process.directory, "cwd").ok()?);
        if !path.is_absolute() || self.cwd.as_ref() == Some(&path) {
            return None;
        }

        // The printed path must identify the same directory in the daemon's namespace.
        let observed = fstatat(&process.directory, "cwd", AtFlags::empty()).ok()?;
        let named = path.metadata().ok()?;
        if !named.is_dir() || named.dev() != observed.st_dev || named.ino() != observed.st_ino {
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

struct ProcessDirectory {
    pid: u32,
    directory: File,
}

impl ProcessDirectory {
    fn open(pid: u32) -> Option<Self> {
        Some(Self {
            pid,
            directory: File::open(format!("/proc/{pid}")).ok()?,
        })
    }

    fn stat(&self) -> Option<Stat> {
        let flags = OFlag::O_RDONLY | OFlag::O_CLOEXEC;
        let fd = openat(&self.directory, "stat", flags, Mode::empty()).ok()?;
        parse_stat_file(File::from(fd), self.pid)
    }

    fn executable(&self) -> Option<Executable> {
        // Read both entries through one directory so PID reuse cannot mix samples.
        let stat = fstatat(&self.directory, "exe", AtFlags::empty()).ok()?;
        (SFlag::from_bits_truncate(stat.st_mode) == SFlag::S_IFREG).then_some(Executable {
            device: stat.st_dev,
            inode: stat.st_ino,
        })
    }
}

fn read_stat(pid: u32) -> Option<Stat> {
    parse_stat_file(File::open(format!("/proc/{pid}/stat")).ok()?, pid)
}

fn parse_stat_file(file: File, pid: u32) -> Option<Stat> {
    let mut bytes = Vec::with_capacity(512);
    file.take(4097).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 4096 {
        return None;
    }
    parse_stat(&bytes, pid)
}

fn parse_stat(bytes: &[u8], pid: u32) -> Option<Stat> {
    let opening = bytes.iter().position(|byte| *byte == b'(')?;
    let closing = bytes.iter().rposition(|byte| *byte == b')')?;
    let read_pid = std::str::from_utf8(bytes.get(..opening)?)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    if read_pid != pid || closing <= opening {
        return None;
    }

    // The command name can contain parentheses, spaces, and non-UTF-8 bytes.
    let fields = std::str::from_utf8(bytes.get(closing + 1..)?).ok()?;
    let mut fields = fields.split_ascii_whitespace();
    let state = fields.next()?;
    let parent_pid = fields.next()?.parse().ok()?;
    let process_group = fields.next()?.parse().ok()?;
    let _session = fields.next()?;
    let tty = fields.next()?.parse().ok()?;
    let foreground_group = fields
        .next()?
        .parse::<i32>()
        .ok()
        .filter(|group| *group > 0);
    let start_ticks = fields.nth(13)?.parse().ok()?;
    Some(Stat {
        parent_pid,
        process_group,
        tty,
        start_ticks,
        foreground_group,
        exited: matches!(state, "Z" | "X" | "x"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
