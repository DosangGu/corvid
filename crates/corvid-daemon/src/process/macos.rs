use super::{Executable, Stat};
use std::ffi::OsStr;
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::PathBuf;

use nix::libc;
use uuid::Uuid;

pub(crate) fn boot_id() -> Option<Uuid> {
    Uuid::parse_str(&corvid_protocol::macos::boot_session().ok()?).ok()
}

// An open procfs directory has no Darwin equivalent. Recheck the process birth
// time around every multi-query sample, and discard it if the PID was reused.
pub(super) struct ProcessDirectory {
    pid: u32,
    started: u64,
}

impl ProcessDirectory {
    pub(super) fn open(pid: u32) -> Option<Self> {
        Self::try_open(pid).ok()
    }

    pub(super) fn try_open(pid: u32) -> io::Result<Self> {
        let stat = stat(pid)?;
        Ok(Self {
            pid,
            started: stat.start_ticks,
        })
    }

    pub(super) fn stat(&self) -> Option<Stat> {
        let stat = read_stat(self.pid)?;
        (stat.start_ticks == self.started).then_some(stat)
    }

    pub(super) fn path(&self) -> Option<PathBuf> {
        self.stat()?;
        let path = corvid_protocol::macos::process_path(self.pid).ok()?;
        self.stat()?;
        Some(path)
    }

    pub(super) fn executable(&self) -> Option<Executable> {
        let image = Executable::read(&self.path()?)?;
        self.stat()?;
        Some(image)
    }

    pub(super) fn cwd(&self) -> Option<PathBuf> {
        self.stat()?;
        let info: libc::proc_vnodepathinfo = query(self.pid, libc::PROC_PIDVNODEPATHINFO).ok()?;
        let bytes: Vec<_> = info
            .pvi_cdir
            .vip_path
            .iter()
            .flatten()
            .map(|byte| *byte as u8)
            .take_while(|byte| *byte != 0)
            .collect();
        let path = PathBuf::from(OsStr::from_bytes(&bytes));
        let named = path.metadata().ok()?;
        let observed = info.pvi_cdir.vip_vi.vi_stat;
        self.stat()?;
        (named.is_dir()
            && named.dev() == u64::from(observed.vst_dev)
            && named.ino() == observed.vst_ino)
            .then_some(path)
    }

    pub(super) fn arguments(&self) -> Option<Vec<u8>> {
        self.stat()?;
        let mut mib = [
            libc::CTL_KERN,
            libc::KERN_PROCARGS2,
            i32::try_from(self.pid).ok()?,
        ];
        let mut bytes = vec![0_u8; 256 * 1024];
        let mut length = bytes.len();
        // KERN_PROCARGS2 returns argc, executable, padding, then NUL-separated argv.
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
        if result != 0 || length > bytes.len() || length < 5 {
            return None;
        }
        bytes.truncate(length);
        let argc = i32::from_ne_bytes(bytes[..4].try_into().ok()?);
        if !(1..=128).contains(&argc) {
            return None;
        }
        let mut offset = 4 + bytes[4..].iter().position(|byte| *byte == 0)?;
        while bytes.get(offset) == Some(&0) {
            offset += 1;
        }
        let start = offset;
        for _ in 0..argc {
            offset += bytes.get(offset..)?.iter().position(|byte| *byte == 0)? + 1;
        }
        self.stat()?;
        (offset - start <= 4096).then(|| bytes[start..offset].to_vec())
    }
}

pub(super) fn read_stat(pid: u32) -> Option<Stat> {
    stat(pid).ok()
}

fn stat(pid: u32) -> io::Result<Stat> {
    let info: libc::proc_bsdinfo = query(pid, libc::PROC_PIDTBSDINFO)?;
    if info.pbi_pid != pid {
        return Err(io::Error::other("process identity changed"));
    }
    Ok(Stat {
        parent_pid: info.pbi_ppid,
        process_group: info.pbi_pgid as i32,
        tty: if info.e_tdev == u32::MAX {
            0
        } else {
            info.e_tdev as i32
        },
        start_ticks: info
            .pbi_start_tvsec
            .checked_mul(1_000_000)
            .and_then(|value| value.checked_add(info.pbi_start_tvusec))
            .ok_or_else(|| io::Error::other("invalid process start time"))?,
        foreground_group: i32::try_from(info.e_tpgid).ok().filter(|group| *group > 0),
        exited: info.pbi_status == 5, // SZOMB, from Darwin's sys/proc.h.
    })
}

fn query<T>(pid: u32, flavor: i32) -> io::Result<T> {
    let pid = i32::try_from(pid).map_err(io::Error::other)?;
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
        let error = io::Error::last_os_error();
        return Err(if error.raw_os_error() == Some(libc::ESRCH) {
            io::Error::new(io::ErrorKind::NotFound, error)
        } else {
            error
        });
    }
    // libproc initialized the complete structure, checked above.
    Ok(unsafe { value.assume_init() })
}
