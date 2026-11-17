use super::inspection::{
    ARGUMENT_LIMIT, BirthIdentity, Observation, ObservationError, ProcessInspector,
    ProcessObservation,
};
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

/// Retains a procfs directory: relative queries cannot follow PID reuse. Exec
/// and directory changes remain possible during this handle's lifetime.
pub(super) struct ProcessDirectory {
    pid: u32,
    pub(super) directory: File,
}

pub(super) struct Inspector;

impl ProcessInspector for Inspector {
    type Handle = ProcessDirectory;

    fn open(&self, pid: u32) -> Observation<Self::Handle> {
        Ok(ProcessDirectory {
            pid,
            directory: File::open(format!("/proc/{pid}"))?,
        })
    }
}

impl ProcessObservation for ProcessDirectory {
    fn path(&self) -> Observation<PathBuf> {
        Ok(PathBuf::from(readlinkat(&self.directory, "exe")?))
    }

    fn arguments(&self) -> Observation<Vec<u8>> {
        let fd = openat(
            &self.directory,
            "cmdline",
            OFlag::O_RDONLY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?;
        let mut bytes = Vec::new();
        File::from(fd)
            .take((ARGUMENT_LIMIT + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > ARGUMENT_LIMIT {
            return Err(ObservationError::TooLarge);
        }
        Ok(bytes)
    }

    fn cwd(&self) -> Observation<PathBuf> {
        let path = PathBuf::from(readlinkat(&self.directory, "cwd")?);
        let observed = fstatat(&self.directory, "cwd", AtFlags::empty())?;
        let named = path.metadata()?;
        (named.is_dir() && named.dev() == observed.st_dev && named.ino() == observed.st_ino)
            .then_some(path)
            .ok_or(ObservationError::Invalid)
    }

    fn stat(&self) -> Observation<Stat> {
        let flags = OFlag::O_RDONLY | OFlag::O_CLOEXEC;
        let fd = openat(&self.directory, "stat", flags, Mode::empty())?;
        let mut bytes = Vec::with_capacity(512);
        File::from(fd).take(4097).read_to_end(&mut bytes)?;
        if bytes.len() > 4096 {
            return Err(ObservationError::TooLarge);
        }
        parse_stat(&bytes, self.pid).ok_or(ObservationError::Invalid)
    }

    fn executable(&self) -> Observation<Executable> {
        // Read both entries through one directory so PID reuse cannot mix samples.
        let stat = fstatat(&self.directory, "exe", AtFlags::empty())?;
        (SFlag::from_bits_truncate(stat.st_mode) == SFlag::S_IFREG)
            .then_some(Executable {
                device: stat.st_dev,
                inode: stat.st_ino,
            })
            .ok_or(ObservationError::Invalid)
    }
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
    let birth = BirthIdentity::from_raw(fields.nth(13)?.parse().ok()?);
    Some(Stat {
        parent_pid,
        process_group,
        tty,
        birth,
        foreground_group,
        exited: matches!(state, "Z" | "X" | "x"),
    })
}
