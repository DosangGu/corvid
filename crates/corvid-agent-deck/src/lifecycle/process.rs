use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::ptr::null_mut;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use compio::fs::named_pipe::NamedPipeClient;
use corvid_async::time::{sleep, timeout};
use corvid_protocol::COMMAND_TIMEOUT;
use windows_sys::Win32::Foundation::{HWND, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetWindowThreadProcessId, IsWindowVisible, PostThreadMessageW, WM_QUIT,
};

pub(super) struct Process {
    handle: OwnedHandle,
    pid: u32,
}

impl Process {
    pub fn from_pipe(pipe: &NamedPipeClient) -> Result<Self> {
        let mut pid = 0;
        // The authenticated private pipe stays open throughout this lookup.
        checked(unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) })?;
        Self::open(pid)
    }

    pub(super) fn open(pid: u32) -> Result<Self> {
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        ensure!(
            !handle.is_null(),
            "cannot open Agent Deck process: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self {
            handle: unsafe { OwnedHandle::from_raw_handle(handle) },
            pid,
        })
    }

    pub fn image_path(&self) -> Result<PathBuf> {
        let mut path = vec![0; 32768];
        let mut length = path.len() as u32;
        checked(unsafe {
            QueryFullProcessImageNameW(
                self.handle.as_raw_handle(),
                0,
                path.as_mut_ptr(),
                &mut length,
            )
        })?;
        Ok(PathBuf::from(OsString::from_wide(&path[..length as usize])))
    }

    fn exited(&self) -> Result<bool> {
        match unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(std::io::Error::last_os_error().into()),
        }
    }

    async fn window(&self) -> Result<HWND> {
        timeout(COMMAND_TIMEOUT, async {
            // GPUI uses this class for the main window, including hidden Decks.
            // Tray windows belong to another thread and must not receive Quit.
            let class: Vec<u16> = "Zed::Window\0".encode_utf16().collect();
            loop {
                ensure!(!self.exited()?, "Agent Deck exited before replacement");
                let mut previous = null_mut();
                loop {
                    let window = unsafe {
                        FindWindowExW(null_mut(), previous, class.as_ptr(), std::ptr::null())
                    };
                    if window.is_null() {
                        break;
                    }
                    let mut pid = 0;
                    unsafe {
                        GetWindowThreadProcessId(window, &mut pid);
                    }
                    if pid == self.pid {
                        return Ok(window);
                    }
                    previous = window;
                }
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("cannot find the running Agent Deck window")?
    }

    pub async fn visible(&self) -> Result<bool> {
        Ok(unsafe { IsWindowVisible(self.window().await?) } != 0)
    }

    pub async fn quit(&self) -> Result<()> {
        let window = self.window().await?;
        let mut pid = 0;
        let thread = unsafe { GetWindowThreadProcessId(window, &mut pid) };
        ensure!(
            pid == self.pid && thread != 0 && !self.exited()?,
            "Agent Deck window changed during replacement"
        );
        // This is GPUI's normal quit path, including its app cleanup callback.
        // WM_CLOSE would merely hide Deck. It also works with older releases
        // which do not have an IPC shutdown command. No force-kill fallback.
        checked(unsafe { PostThreadMessageW(thread, WM_QUIT, 0, 0) })?;
        timeout(COMMAND_TIMEOUT, async {
            while !self.exited()? {
                sleep(Duration::from_millis(50)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("old Agent Deck did not exit for the update")?
    }
}

fn checked(result: i32) -> Result<()> {
    if result == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
