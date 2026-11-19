use super::inspection::{
    ARGUMENT_LIMIT, BirthIdentity, Observation, ObservationError, ProcessInspector,
    ProcessObservation,
};
use super::{Executable, Stat};
use std::ffi::{OsStr, OsString};
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::ops::Range;
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::PathBuf;

use nix::libc;
use uuid::Uuid;

pub(crate) fn boot_id() -> Option<Uuid> {
    Uuid::parse_str(&corvid_protocol::macos::boot_session().ok()?).ok()
}

/// Darwin has no retained procfs directory. Bind the PID/birth pair and recheck
/// it around multi-query observations. This rejects PID reuse, but does not
/// freeze the executable, arguments, or CWD against an in-process change.
pub(super) struct ProcessDirectory {
    pid: u32,
    started: BirthIdentity,
}

pub(super) struct Inspector;

impl ProcessInspector for Inspector {
    type Handle = ProcessDirectory;

    fn open(&self, pid: u32) -> Observation<Self::Handle> {
        let stat = stat(pid)?;
        Ok(ProcessDirectory {
            pid,
            started: stat.birth,
        })
    }
}

impl ProcessObservation for ProcessDirectory {
    fn stat(&self) -> Observation<Stat> {
        let stat = stat(self.pid)?;
        stat.birth.verify(self.started)?;
        Ok(stat)
    }

    fn path(&self) -> Observation<PathBuf> {
        self.stat()?;
        let path = corvid_protocol::unix::process_image_path(self.pid)?;
        self.stat()?;
        Ok(path)
    }

    fn executable(&self) -> Observation<Executable> {
        let image = Executable::read(&self.path()?).ok_or(ObservationError::Unavailable)?;
        self.stat()?;
        Ok(image)
    }

    fn cwd(&self) -> Observation<PathBuf> {
        self.stat()?;
        let info: libc::proc_vnodepathinfo = query(self.pid, libc::PROC_PIDVNODEPATHINFO)?;
        let bytes: Vec<_> = info
            .pvi_cdir
            .vip_path
            .iter()
            .flatten()
            .map(|byte| *byte as u8)
            .take_while(|byte| *byte != 0)
            .collect();
        let path = PathBuf::from(OsStr::from_bytes(&bytes));
        let named = path.metadata()?;
        let observed = info.pvi_cdir.vip_vi.vi_stat;
        self.stat()?;
        (named.is_dir()
            && named.dev() == u64::from(observed.vst_dev)
            && named.ino() == observed.vst_ino)
            .then_some(path)
            .ok_or(ObservationError::Invalid)
    }

    fn arguments(&self) -> Observation<Vec<u8>> {
        let bytes = self.procargs()?;
        Ok(bytes[argument_range(&bytes)?].to_vec())
    }
}

impl ProcessDirectory {
    fn procargs(&self) -> Observation<Vec<u8>> {
        self.stat()?;
        let mut mib = [
            libc::CTL_KERN,
            libc::KERN_PROCARGS2,
            i32::try_from(self.pid).map_err(|_| ObservationError::Invalid)?,
        ];
        // Keep the existing finite sysctl buffer, including executable/padding
        // and environment; only the bounded argv slice is ever returned.
        let mut bytes = vec![0_u8; 256 * 1024];
        let mut length = bytes.len();
        let result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                bytes.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        if length > bytes.len() {
            return Err(ObservationError::TooLarge);
        }
        bytes.truncate(length);
        self.stat()?;
        Ok(bytes)
    }

    pub(super) fn search_path(&self) -> Option<OsString> {
        let bytes = self.procargs().ok()?;
        let end = argument_range(&bytes).ok()?.end;
        super::environment_path(&bytes[end..])
    }
}

/// KERN_PROCARGS2 contains argc, executable, padding, argv, then environment.
fn argument_range(bytes: &[u8]) -> Observation<Range<usize>> {
    let invalid = ObservationError::Invalid;
    let argc = i32::from_ne_bytes(
        bytes
            .get(..4)
            .ok_or(invalid)?
            .try_into()
            .map_err(|_| invalid)?,
    );
    if !(1..=128).contains(&argc) {
        return Err(invalid);
    }
    let mut offset = 4 + bytes[4..]
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(invalid)?;
    while bytes.get(offset) == Some(&0) {
        offset += 1;
    }
    let start = offset;
    for _ in 0..argc {
        offset += bytes
            .get(offset..)
            .ok_or(invalid)?
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(invalid)?
            + 1;
        if offset - start > ARGUMENT_LIMIT {
            return Err(ObservationError::TooLarge);
        }
    }
    Ok(start..offset)
}

fn stat(pid: u32) -> Observation<Stat> {
    let info: libc::proc_bsdinfo = query(pid, libc::PROC_PIDTBSDINFO)?;
    if info.pbi_pid != pid {
        return Err(ObservationError::Changed);
    }
    let started = info
        .pbi_start_tvsec
        .checked_mul(1_000_000)
        .and_then(|value| value.checked_add(info.pbi_start_tvusec))
        .ok_or(ObservationError::Invalid)?;
    Ok(Stat {
        parent_pid: info.pbi_ppid,
        process_group: info.pbi_pgid as i32,
        tty: if info.e_tdev == u32::MAX {
            0
        } else {
            info.e_tdev as i32
        },
        birth: BirthIdentity::from_raw(started),
        foreground_group: i32::try_from(info.e_tpgid).ok().filter(|group| *group > 0),
        exited: info.pbi_status == 5, // SZOMB, from Darwin's sys/proc.h.
    })
}

fn query<T>(pid: u32, flavor: i32) -> Observation<T> {
    let pid = i32::try_from(pid).map_err(|_| ObservationError::Invalid)?;
    let mut value = MaybeUninit::<T>::zeroed();
    // All callers pair a libproc flavor with its SDK-defined C structure.
    let count = unsafe {
        libc::proc_pidinfo(
            pid,
            flavor,
            0,
            value.as_mut_ptr().cast(),
            size_of::<T>() as i32,
        )
    };
    if count != size_of::<T>() as i32 {
        // A short successful response is inconclusive; errno can be stale.
        return Err(
            if count <= 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                ObservationError::Gone
            } else {
                ObservationError::Unavailable
            },
        );
    }
    // libproc initialized the complete structure, checked above.
    Ok(unsafe { value.assume_init() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn procargs(argv: &[u8], argc: i32) -> Vec<u8> {
        let mut bytes = argc.to_ne_bytes().to_vec();
        bytes.extend_from_slice(b"/fixture\0\0");
        bytes.extend_from_slice(argv);
        bytes.extend_from_slice(b"PRIVATE_ENV=not-an-argument\0");
        bytes
    }

    #[test]
    fn arguments_exclude_environment_and_reject_malformed_or_oversized_argv() {
        let parse_arguments =
            |bytes: &[u8]| argument_range(bytes).map(|range| bytes[range].to_vec());
        assert_eq!(
            parse_arguments(&procargs(b"agent\0run\0", 2)),
            Ok(b"agent\0run\0".to_vec())
        );
        let mut limit = vec![b'a'; ARGUMENT_LIMIT - 1];
        limit.push(0);
        assert_eq!(parse_arguments(&procargs(&limit, 1)), Ok(limit));
        let mut oversized = vec![b'a'; ARGUMENT_LIMIT];
        oversized.push(0);
        assert_eq!(
            parse_arguments(&procargs(&oversized, 1)),
            Err(ObservationError::TooLarge)
        );
        for bytes in [
            vec![],
            vec![0; 3],
            procargs(b"agent\0", 0),
            procargs(b"agent\0", 129),
            1_i32.to_ne_bytes().to_vec(),
        ] {
            assert_eq!(parse_arguments(&bytes), Err(ObservationError::Invalid));
        }
    }
}
