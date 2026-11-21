use std::fs::OpenOptions;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use std::process::Stdio;

use anyhow::{Context, Result};
use compio::driver::{ToSharedFd, op::ConnectNamedPipe};
use compio::runtime::fd::AsyncFd;
use windows_sys::Win32::Foundation::{
    ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
    SECURITY_IDENTIFICATION,
};
use windows_sys::Win32::System::Pipes::{
    CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES,
};

use super::{PrivateDescriptor, current_user, pipe_name, verify_private};

pub struct CompioPipeListener {
    name: String,
    user: String,
    waiting: AsyncFd<OwnedHandle>,
}

impl CompioPipeListener {
    pub fn bind_scoped(prefix: &str, instance: &str) -> Result<Self> {
        let user = current_user()?;
        let name = pipe_name(prefix, &user, instance)?;
        let waiting = listen(&name, &user, true)?;
        Ok(Self {
            name,
            user,
            waiting,
        })
    }

    pub async fn accept(&mut self) -> Result<AsyncFd<OwnedHandle>> {
        loop {
            let result =
                compio::runtime::submit(ConnectNamedPipe::new(self.waiting.to_shared_fd()))
                    .await
                    .0;
            let next = listen(&self.name, &self.user, false)?;
            let accepted = std::mem::replace(&mut self.waiting, next);
            match result {
                Ok(_) => return Ok(accepted),
                Err(error)
                    if matches!(
                        error.raw_os_error().map(|code| code as u32),
                        Some(ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED)
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}

/// A private synchronous child stream paired with an overlapped parent stream.
pub async fn child_stdio_pipe(child_reads: bool) -> Result<(AsyncFd<OwnedHandle>, Stdio)> {
    let user = current_user()?;
    let instance = uuid::Uuid::new_v4().simple().to_string();
    let name = pipe_name("corvid-child-stdio", &user, &instance)?;
    let parent = listen(&name, &user, true)?;
    let child = OpenOptions::new()
        .read(child_reads)
        .write(!child_reads)
        .security_qos_flags(SECURITY_IDENTIFICATION)
        .open(&name)
        .context("opening private child stdio")?;
    verify_private(child.as_raw_handle(), &user, false)?;
    compio::runtime::submit(ConnectNamedPipe::new(parent.to_shared_fd()))
        .await
        .0
        .context("connecting private child stdio")?;
    Ok((parent, Stdio::from(child)))
}

fn listen(name: &str, user: &str, first: bool) -> Result<AsyncFd<OwnedHandle>> {
    let name: Vec<_> = std::ffi::OsStr::new(name)
        .encode_wide()
        .chain([0])
        .collect();
    let descriptor = PrivateDescriptor::new(user, false)?;
    let attributes = descriptor.attributes();
    let mut access = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        access |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    // Both the terminated name and the security descriptor outlive this call.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            access,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            65_536,
            65_536,
            0,
            &attributes,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error()).context("opening private pipe");
    }
    // CreateNamedPipeW returned an owned overlapped handle.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    verify_private(handle.as_raw_handle(), user, false).context("verifying pipe ACL")?;
    AsyncFd::new(handle).context("attaching private pipe to Compio")
}
