use std::fs::{self, File};
use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use corvid_core::agent::ProcessLiveness;
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
    start_ticks: u64,
    foreground_group: Option<i32>,
    exited: bool,
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
    pub(crate) fn new(pid: u32, boot_id: Uuid, terminal_id: Uuid) -> Self {
        let stagger = Duration::from_millis((terminal_id.as_u128() % 2_000) as u64);
        Self {
            pid,
            boot_id,
            next: Instant::now() + stagger,
            identity: None,
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
        let identity = Incarnation {
            boot_id: self.boot_id,
            pid: self.pid,
            start_ticks: stat.start_ticks,
            foreground_group: stat.foreground_group,
        };
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
    let foreground_group = fields
        .nth(4)?
        .parse::<i32>()
        .ok()
        .filter(|group| *group > 0);
    let start_ticks = fields.nth(13)?.parse().ok()?;
    Some(Stat {
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
