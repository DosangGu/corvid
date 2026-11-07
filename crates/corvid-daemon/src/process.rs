use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use corvid_core::agent::{ProcessLiveness, Provider};
use corvid_protocol::collector::CODEX_VERSION;
use nix::unistd::geteuid;
use uuid::Uuid;

const PROBE_SPACING: Duration = Duration::from_millis(50);
const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(crate) struct ProbeBudget(Mutex<Option<Instant>>);

impl ProbeBudget {
    fn acquire(&self, now: Instant) -> bool {
        let mut next = self.0.lock().expect("probe budget lock poisoned");
        if next.is_some_and(|deadline| now < deadline) {
            return false;
        }
        *next = Some(now + PROBE_SPACING);
        true
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
pub(crate) struct HookOwner {
    pub(crate) incarnation: Incarnation,
    pub(crate) executable: Executable,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct HookProducer {
    pub(crate) pid: u32,
    pub(crate) start_ticks: u64,
    pub(crate) executable: Executable,
}

impl HookOwner {
    /// Scope an observer to the managed launcher or its qualified local broker.
    /// Tool descendants may inherit the launch association without owning it.
    pub(crate) fn verify_peer(self, peer: u32, provider: Provider) -> Option<HookProducer> {
        let pid = self.incarnation.pid;
        let launcher = read_stat(pid)?;
        if launcher.exited
            || launcher.start_ticks != self.incarnation.start_ticks
            || Executable::read(Path::new(&format!("/proc/{pid}/exe"))) != Some(self.executable)
        {
            return None;
        }
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
    pid: u32,
    boot_id: Uuid,
    next: Instant,
    identity: Option<Incarnation>,
}

impl Probe {
    pub(crate) fn new(
        pid: u32,
        boot_id: Uuid,
        terminal_id: Uuid,
        identity: Option<Incarnation>,
    ) -> Self {
        let stagger = Duration::from_millis((terminal_id.as_u128() % 2_000) as u64);
        Self {
            pid,
            boot_id,
            next: Instant::now() + stagger,
            identity,
        }
    }

    pub(crate) fn sample(
        &mut self,
        budget: &ProbeBudget,
    ) -> Option<(ProcessLiveness, Option<Incarnation>)> {
        let now = Instant::now();
        if now < self.next || !budget.acquire(now) {
            return None;
        }
        self.next = now + SAMPLE_INTERVAL;

        let Some(stat) = read_stat(self.pid) else {
            return Some((ProcessLiveness::Unknown, None));
        };
        if self
            .identity
            .is_some_and(|identity| identity.start_ticks != stat.start_ticks)
        {
            return Some((ProcessLiveness::Unknown, None));
        }
        let identity = stat.incarnation(self.pid, self.boot_id);
        self.identity = Some(identity);
        let liveness = if stat.exited {
            ProcessLiveness::Exited { code: None }
        } else {
            ProcessLiveness::Running
        };
        Some((liveness, Some(identity)))
    }
}

fn read_stat(pid: u32) -> Option<Stat> {
    let mut bytes = Vec::with_capacity(512);
    File::open(format!("/proc/{pid}/stat"))
        .ok()?
        .take(4097)
        .read_to_end(&mut bytes)
        .ok()?;
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
    let foreground_group = fields
        .nth(3)?
        .parse::<i32>()
        .ok()
        .filter(|group| *group > 0);
    let start_ticks = fields.nth(13)?.parse().ok()?;
    Some(Stat {
        parent_pid,
        start_ticks,
        foreground_group,
        exited: matches!(state, "Z" | "X" | "x"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
