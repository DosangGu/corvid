use super::{Executable, Stat};
use nix::fcntl::{AtFlags, OFlag, openat, readlinkat};
use nix::sys::stat::{Mode, SFlag, fstatat};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use uuid::Uuid;

pub(crate) fn boot_id() -> Option<Uuid> {
    Uuid::parse_str(
        fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()?
            .trim(),
    )
    .ok()
}

pub(super) struct ProcessDirectory {
    pid: u32,
    pub(super) directory: File,
}

impl ProcessDirectory {
    pub(super) fn open(pid: u32) -> Option<Self> {
        Some(Self {
            pid,
            directory: File::open(format!("/proc/{pid}")).ok()?,
        })
    }

    pub(super) fn try_open(pid: u32) -> std::io::Result<Self> {
        Ok(Self {
            pid,
            directory: File::open(format!("/proc/{pid}"))?,
        })
    }

    pub(super) fn path(&self) -> Option<PathBuf> {
        Some(PathBuf::from(readlinkat(&self.directory, "exe").ok()?))
    }

    pub(super) fn arguments(&self) -> Option<Vec<u8>> {
        let fd = openat(
            &self.directory,
            "cmdline",
            OFlag::O_RDONLY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .ok()?;
        let mut bytes = Vec::new();
        File::from(fd).take(4097).read_to_end(&mut bytes).ok()?;
        Some(bytes)
    }

    pub(super) fn cwd(&self) -> Option<PathBuf> {
        let path = PathBuf::from(readlinkat(&self.directory, "cwd").ok()?);
        let observed = fstatat(&self.directory, "cwd", AtFlags::empty()).ok()?;
        let named = path.metadata().ok()?;
        (named.is_dir() && named.dev() == observed.st_dev && named.ino() == observed.st_ino)
            .then_some(path)
    }

    pub(super) fn stat(&self) -> Option<Stat> {
        let flags = OFlag::O_RDONLY | OFlag::O_CLOEXEC;
        let fd = openat(&self.directory, "stat", flags, Mode::empty()).ok()?;
        parse_stat_file(File::from(fd), self.pid)
    }

    pub(super) fn executable(&self) -> Option<Executable> {
        // Read both entries through one directory so PID reuse cannot mix samples.
        let stat = fstatat(&self.directory, "exe", AtFlags::empty()).ok()?;
        (SFlag::from_bits_truncate(stat.st_mode) == SFlag::S_IFREG).then_some(Executable {
            device: stat.st_dev,
            inode: stat.st_ino,
        })
    }
}

pub(super) fn read_stat(pid: u32) -> Option<Stat> {
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

pub(super) fn parse_stat(bytes: &[u8], pid: u32) -> Option<Stat> {
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
