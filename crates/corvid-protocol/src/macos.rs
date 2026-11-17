//! Darwin OS queries shared by the local transport and its clients.
use std::ffi::{CStr, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use nix::libc;

pub fn login_shell() -> Option<PathBuf> {
    nix::unistd::User::from_uid(nix::unistd::geteuid())
        .ok()?
        .map(|user| user.shell)
}

pub fn boot_session() -> io::Result<String> {
    let mut bytes = [0_u8; 128];
    let mut length = bytes.len();
    // sysctl writes at most the supplied length into this live buffer.
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let value =
        CStr::from_bytes_until_nul(&bytes[..length.min(bytes.len())]).map_err(io::Error::other)?;
    Ok(value.to_str().map_err(io::Error::other)?.to_owned())
}

pub fn process_path(pid: u32) -> io::Result<PathBuf> {
    let pid = i32::try_from(pid).map_err(io::Error::other)?;
    let mut bytes = [0_u8; 4096];
    // proc_pidpath receives the exact capacity of this initialized buffer.
    let length = unsafe { libc::proc_pidpath(pid, bytes.as_mut_ptr().cast(), bytes.len() as u32) };
    if length <= 0 {
        return Err(io::Error::last_os_error());
    }
    let path = CStr::from_bytes_until_nul(&bytes).map_err(io::Error::other)?;
    Ok(PathBuf::from(OsStr::from_bytes(path.to_bytes())))
}
