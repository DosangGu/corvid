use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use corvid_core::agent::{ProcessLiveness, Provider};
use nix::unistd::geteuid;
use semver::Version;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod inspection;
#[cfg_attr(target_os = "linux", path = "process/linux.rs")]
#[cfg_attr(target_os = "macos", path = "process/macos.rs")]
mod native;

pub(crate) use inspection::BirthIdentity;
use inspection::{Clock, ObservationError, ProcessInspector, ProcessObservation, SystemClock};
use native::Inspector as NativeInspector;
pub(crate) use native::boot_id;
#[cfg(target_os = "linux")]
pub(crate) use native::legacy_observer;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct Incarnation {
    pub(crate) boot_id: Uuid,
    pub(crate) pid: u32,
    pub(crate) birth: BirthIdentity,
    pub(crate) foreground_group: Option<i32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stat {
    parent_pid: u32,
    process_group: i32,
    tty: i32,
    birth: BirthIdentity,
    foreground_group: Option<i32>,
    exited: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct ProcessImage {
    pub(crate) incarnation: Incarnation,
    pub(crate) executable: Executable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct HookProducer {
    pub(crate) pid: u32,
    pub(crate) birth: BirthIdentity,
    pub(crate) executable: Executable,
}

impl ProcessImage {
    #[cfg(test)]
    pub(crate) fn capture(incarnation: Incarnation, expected: Option<Executable>) -> Option<Self> {
        Self::capture_with(&NativeInspector, incarnation, expected)
    }

    fn capture_with(
        inspector: &impl ProcessInspector,
        incarnation: Incarnation,
        expected: Option<Executable>,
    ) -> Option<Self> {
        let process = inspector.open(incarnation.pid).ok()?;
        let stat = process.stat().ok()?;
        stat.birth.verify(incarnation.birth).ok()?;
        if stat.exited {
            return None;
        }
        let executable = process.executable().ok()?;
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
        Self::capture_started_with(&NativeInspector, &SystemClock, incarnation, expected)
    }

    fn capture_started_with(
        inspector: &impl ProcessInspector,
        clock: &impl Clock,
        incarnation: Incarnation,
        expected: Option<Executable>,
    ) -> Option<Self> {
        // portable-pty can return before exec. Keep waiting policy independent
        // of the OS, and never bind the helper or a different qualified image.
        let helper = inspector.helper_image()?;
        let deadline = clock.now() + Duration::from_millis(250);
        let mut candidate = None;
        while clock.now() < deadline {
            if let Some(image) = Self::capture_with(inspector, incarnation, None)
                && image.executable != helper
            {
                if expected == Some(image.executable) {
                    return Some(image);
                }
                if expected.is_none() {
                    let now = clock.now();
                    match candidate {
                        Some((executable, sampled)) if executable == image.executable => {
                            if now.duration_since(sampled) >= Duration::from_millis(20) {
                                return Some(image);
                            }
                        }
                        _ => candidate = Some((image.executable, now)),
                    }
                }
            } else {
                // An interrupted query breaks the stable-image interval.
                candidate = None;
            }
            clock.sleep(Duration::from_millis(2));
        }
        None
    }

    pub(crate) fn is_current(self) -> bool {
        self.current_stat_with(&NativeInspector).is_some()
    }

    pub(crate) fn belongs_to(self, root: Incarnation) -> bool {
        if root.boot_id != self.incarnation.boot_id || boot_id() != Some(root.boot_id) {
            return false;
        }
        let Some(owner) = self.current_stat_with(&NativeInspector) else {
            return false;
        };
        let Some(root_stat) = NativeInspector
            .open(root.pid)
            .ok()
            .and_then(|process| process.stat().ok())
        else {
            return false;
        };
        !root_stat.exited
            && root_stat.birth == root.birth
            && owner.tty == root_stat.tty
            && (self.incarnation.pid == root.pid || owner.parent_pid == root.pid)
    }

    pub(crate) fn shell_exit(
        self,
        pending: bool,
        restart_path: Option<&Path>,
    ) -> Option<ProcessLiveness> {
        self.shell_exit_with(&NativeInspector, pending, restart_path)
    }

    fn shell_exit_with(
        self,
        inspector: &impl ProcessInspector,
        pending: bool,
        restart_path: Option<&Path>,
    ) -> Option<ProcessLiveness> {
        let pid = self.incarnation.pid;
        let directory = match inspector.open(pid) {
            Ok(directory) => directory,
            Err(ObservationError::Gone) => {
                return Some(ProcessLiveness::Exited { code: None });
            }
            Err(_) => return Some(ProcessLiveness::Unknown),
        };
        let stat = match directory.stat() {
            Ok(stat) => stat,
            Err(_) if restart_path.is_some() => return None,
            Err(_) => return Some(ProcessLiveness::Unknown),
        };
        if stat.exited || stat.birth != self.incarnation.birth {
            return Some(ProcessLiveness::Exited { code: None });
        }
        let image = match directory.executable() {
            Ok(image) => Some(image),
            // The executable can briefly be unavailable during exec. The
            // next sample or a verified startup will resolve the transition.
            Err(_) if restart_path.is_some() => return None,
            Err(_) => None,
        };
        let starting = pending && image.is_some() && image == inspector.helper_image();
        (image != Some(self.executable)
            && !starting
            && !image.is_some_and(|image| matches_launcher(restart_path, image)))
        .then_some(ProcessLiveness::Unknown)
    }

    fn current_stat_with(self, inspector: &impl ProcessInspector) -> Option<Stat> {
        let process = inspector.open(self.incarnation.pid).ok()?;
        let stat = process.stat().ok()?;
        (!stat.exited
            && stat.birth == self.incarnation.birth
            && process.executable().ok() == Some(self.executable))
        .then_some(stat)
    }

    pub(crate) fn verify_hook(
        self,
        peer: u32,
        provider: Provider,
        restart_path: Option<&Path>,
    ) -> Option<(Self, HookProducer)> {
        if let Some(producer) = self.verify_peer_with(&NativeInspector, peer, provider) {
            return Some((self, producer));
        }
        let owner = self.verify_restart_with(&NativeInspector, peer, restart_path?)?;
        let producer = owner.verify_peer_with(&NativeInspector, peer, provider)?;
        Some((owner, producer))
    }

    fn verify_restart_with(
        self,
        inspector: &impl ProcessInspector,
        peer: u32,
        launcher: &Path,
    ) -> Option<Self> {
        let pid = self.incarnation.pid;
        let observer = inspector.open(peer).ok()?.stat().ok()?;
        if observer.exited || observer.parent_pid != pid {
            return None;
        }
        let process = inspector.open(pid).ok()?;
        let stat = process.stat().ok()?;
        stat.birth.verify(self.incarnation.birth).ok()?;
        if stat.exited || stat.tty == 0 || stat.foreground_group != Some(stat.process_group) {
            return None;
        }
        let executable = process.executable().ok()?;
        (executable != self.executable && matches_launcher(Some(launcher), executable)).then(|| {
            Self {
                incarnation: stat.incarnation(pid, self.incarnation.boot_id),
                executable,
            }
        })
    }

    /// Scope an observer to the managed launcher or its qualified local broker.
    /// Tool descendants may inherit the launch association without owning it.
    fn verify_peer_with(
        self,
        inspector: &impl ProcessInspector,
        peer: u32,
        provider: Provider,
    ) -> Option<HookProducer> {
        let pid = self.incarnation.pid;
        let launcher = self.current_stat_with(inspector)?;
        let observer = inspector.open(peer).ok()?.stat().ok()?;
        if observer.exited {
            return None;
        }
        if observer.parent_pid == pid {
            return Some(HookProducer {
                pid,
                birth: launcher.birth,
                executable: self.executable,
            });
        }
        if !crate::provider::contract(provider).unix.managed_broker {
            return None;
        }
        codex_broker(inspector, observer.parent_pid, pid)
    }
}

fn matches_launcher(launcher: Option<&Path>, image: Executable) -> bool {
    launcher.is_some_and(|path| Executable::read(path) == Some(image))
}

pub(crate) fn verify_shell_launcher(shell: Incarnation, peer: u32) -> Option<Incarnation> {
    verify_shell_launcher_with(&NativeInspector, shell, peer)
}

fn verify_shell_launcher_with(
    inspector: &impl ProcessInspector,
    shell: Incarnation,
    peer: u32,
) -> Option<Incarnation> {
    let root = inspector.open(shell.pid).ok()?.stat().ok()?;
    if root.exited || root.birth != shell.birth || root.tty == 0 {
        return None;
    }
    let process = inspector.open(peer).ok()?;
    let stat = process.stat().ok()?;
    if stat.exited
        || (peer != shell.pid && stat.parent_pid != shell.pid)
        || (peer == shell.pid && stat.birth != shell.birth)
        || stat.tty != root.tty
        || stat.foreground_group != Some(stat.process_group)
        || process.executable().ok()? != inspector.helper_image()?
    {
        return None;
    }
    Some(stat.incarnation(peer, shell.boot_id))
}

fn codex_broker(
    inspector: &impl ProcessInspector,
    pid: u32,
    launcher: u32,
) -> Option<HookProducer> {
    let process = inspector.open(pid).ok()?;
    let stat = process.stat().ok()?;
    if stat.exited || stat.parent_pid != launcher {
        return None;
    }
    let image = process.path().ok()?;
    let target = if cfg!(target_os = "macos") {
        "aarch64-apple-darwin"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64-unknown-linux-musl"
    } else {
        "x86_64-unknown-linux-musl"
    };
    if !codex_broker_path(&image, target) {
        return None;
    }
    let metadata = image.metadata().ok()?;
    if metadata.uid() != geteuid().as_raw() || !metadata.is_file() || metadata.mode() & 0o022 != 0 {
        return None;
    }
    let args = process.arguments().ok()?;
    if args.len() > inspection::ARGUMENT_LIMIT || !codex_broker_args(&args) {
        return None;
    }
    let executable = process.executable().ok()?;
    if executable
        != (Executable {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    {
        return None;
    }
    let after = process.stat().ok()?;
    if after.exited
        || after.birth != stat.birth
        || after.parent_pid != launcher
        || Executable::read(&image) != Some(executable)
    {
        return None;
    }
    Some(HookProducer {
        pid,
        birth: stat.birth,
        executable,
    })
}

fn codex_broker_path(image: &Path, target: &str) -> bool {
    if !image.ends_with("bin/codex") {
        return false;
    }
    let Some(release) = image.parent().and_then(Path::parent) else {
        return false;
    };
    if !release
        .parent()
        .is_some_and(|parent| parent.ends_with("packages/app-server-daemon/releases"))
    {
        return false;
    }
    let suffix = format!("-{target}");
    release
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(&suffix))
        .is_some_and(|version| Version::parse(version).is_ok())
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
    let stat = NativeInspector.open(pid).ok()?.stat().ok()?;
    (!stat.exited).then(|| stat.incarnation(pid, boot_id))
}

/// Checked on a wheel history request, including after the shell execs another image.
pub(crate) fn interactive_shell(pid: u32, marked_prompt: bool) -> bool {
    let Ok(process) = NativeInspector.open(pid) else {
        return false;
    };
    let Ok(path) = process.path() else {
        return false;
    };
    let name = path.file_name().and_then(|name| name.to_str());
    let known_shell = matches!(
        name,
        Some(
            "sh" | "ash"
                | "bash"
                | "dash"
                | "zsh"
                | "ksh"
                | "mksh"
                | "fish"
                | "csh"
                | "tcsh"
                | "nu"
                | "pwsh"
        )
    );
    if !known_shell
        && std::env::var_os("SHELL")
            .and_then(|shell| fs::canonicalize(shell).ok())
            .is_none_or(|shell| shell != path)
    {
        return false;
    }
    if marked_prompt {
        return true;
    }
    let Ok(argv) = process.arguments() else {
        return false;
    };
    let mut args = argv.split(|&byte| byte == 0);
    args.next();
    args.filter(|arg| !arg.is_empty()).all(|arg| {
        matches!(
            arg,
            b"--login" | b"--interactive" | b"--noprofile" | b"--norc" | b"--no-config"
        ) || arg.strip_prefix(b"-").is_some_and(|flags| {
            !flags.is_empty() && flags.iter().all(|flag| b"ilf".contains(flag))
        })
    })
}

impl Stat {
    fn incarnation(&self, pid: u32, boot_id: Uuid) -> Incarnation {
        Incarnation {
            boot_id,
            pid,
            birth: self.birth,
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
    restart_path: Option<Arc<Path>>,
    cwd: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
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
            restart_path: None,
            cwd: None,
        }
    }

    pub(crate) fn allow_restart(&mut self, launcher: Option<Arc<Path>>) {
        self.restart_path = launcher;
    }

    pub(crate) fn sample(&mut self, output: bool) -> Option<(ProbeResult, Option<PathBuf>)> {
        self.sample_with(&NativeInspector, output, Instant::now())
    }

    fn sample_with(
        &mut self,
        inspector: &impl ProcessInspector,
        output: bool,
        now: Instant,
    ) -> Option<(ProbeResult, Option<PathBuf>)> {
        if output && now >= self.output_after {
            self.next = self.next.min(now);
        }
        if now < self.next || !self.budget.acquire(self.terminal, now) {
            return None;
        }
        self.next = now + SAMPLE_INTERVAL;
        self.output_after = now + OUTPUT_DEBOUNCE;
        Some(self.read_with(inspector))
    }

    fn read_with(&mut self, inspector: &impl ProcessInspector) -> (ProbeResult, Option<PathBuf>) {
        let unknown = (ProbeResult::Status(ProcessLiveness::Unknown, None), None);
        let Ok(process) = inspector.open(self.pid) else {
            return unknown;
        };
        let Ok(stat) = process.stat() else {
            return unknown;
        };
        if self
            .identity
            .is_some_and(|identity| identity.birth != stat.birth)
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
            let Ok(executable) = process.executable() else {
                return (unknown.0, cwd);
            };
            if executable != expected {
                if !matches_launcher(self.restart_path.as_deref(), executable) {
                    self.executable = None;
                    return (ProbeResult::ImageChanged(identity), cwd);
                }
                // The startup hook independently authenticates the new image and
                // establishes its run. The probe only keeps tracking this PTY.
                self.executable = Some(executable);
            }
        }
        (
            ProbeResult::Status(ProcessLiveness::Running, Some(identity)),
            cwd,
        )
    }

    fn changed_cwd(&mut self, process: &impl ProcessObservation) -> Option<PathBuf> {
        let path = process.cwd().ok()?;
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
mod scripted_tests;
#[cfg(test)]
mod tests;
