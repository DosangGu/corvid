use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr::null;

use anyhow::{Result, ensure};
use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CreateProcessW, DETACHED_PROCESS,
    GetExitCodeProcess, OpenProcess, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};

pub(super) struct DaemonProcess(OwnedHandle);

impl DaemonProcess {
    pub(super) async fn open(runtime: &Path) -> Result<Self> {
        let pipe = corvid_protocol::windows::DaemonPipe::Control
            .connect(runtime)
            .await?;
        let mut pid = 0;
        // The private pipe stays open while its server identity is queried.
        if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if process.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        // The handle remains tied to this process across PID reuse.
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(process) }))
    }

    pub(super) fn spawn(exe: &Path, runtime: &Path, state: &Path, ssh: bool) -> Result<Self> {
        let application: Vec<_> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut command = Vec::new();
        append_path(&mut command, exe)?;
        command.extend(" daemon --autostarted --runtime-dir ".encode_utf16());
        append_path(&mut command, runtime)?;
        command.extend(" --state-dir ".encode_utf16());
        append_path(&mut command, state)?;
        command.push(0);

        let mut flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        if ssh {
            flags |= CREATE_BREAKAWAY_FROM_JOB;
        }
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        // The two strings are NUL-terminated and the command buffer is mutable.
        // Inherit no handles so a detached daemon cannot keep SSH's stdio open.
        let started = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command.as_mut_ptr(),
                null(),
                null(),
                0,
                flags,
                null(),
                null(),
                &startup,
                &mut process,
            )
        };
        if started == 0 {
            return Err(io::Error::last_os_error().into());
        }
        // On success Windows returns two owned handles.
        let _thread = unsafe { OwnedHandle::from_raw_handle(process.hThread) };
        Ok(Self(unsafe {
            OwnedHandle::from_raw_handle(process.hProcess)
        }))
    }

    pub(super) fn exit_code(&self) -> io::Result<Option<u32>> {
        let handle = self.0.as_raw_handle();
        // The process handle remains valid while this wrapper is borrowed.
        match unsafe { WaitForSingleObject(handle, 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                // The output points to a writable exit-code value.
                if unsafe { GetExitCodeProcess(handle, &mut code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Some(code))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub(super) fn terminate(&self) {
        let handle = self.0.as_raw_handle();
        // Only the process created by this startup attempt can be terminated.
        unsafe {
            TerminateProcess(handle, 1);
            WaitForSingleObject(handle, 1000);
        }
    }
}

fn append_path(command: &mut Vec<u16>, path: &Path) -> Result<()> {
    let quote = u16::from(b'"');
    let slash = u16::from(b'\\');
    let path: Vec<_> = path.as_os_str().encode_wide().collect();
    ensure!(
        !path.iter().any(|unit| *unit == 0 || *unit == quote),
        "invalid Windows daemon path"
    );
    // Double trailing backslashes so the closing quote remains a delimiter.
    let trailing_slashes = path.iter().rev().take_while(|unit| **unit == slash).count();
    command.push(quote);
    command.extend(path);
    command.extend(std::iter::repeat_n(slash, trailing_slashes));
    command.push(quote);
    Ok(())
}
