use std::os::windows::io::OwnedHandle;
use std::time::Duration;

use anyhow::Result;
use tokio::time::sleep;
use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetClassNameW, GetForegroundWindow, GetWindow, GetWindowThreadProcessId,
    IsIconic, IsWindowVisible, SW_RESTORE, SetForegroundWindow, ShowWindowAsync,
};
use windows_sys::core::BOOL;

use super::{checked, process_alive};

const WINDOW_CLASS: &str = "org.wezfurlong.wezterm";

pub(super) async fn activate(pid: u32, process: &OwnedHandle) -> Result<bool> {
    let Some(window) = request_foreground(pid, process)? else {
        return Ok(false);
    };
    for _ in 0..5 {
        if !process_alive(process) || window_pid(window as HWND) != pid {
            return Ok(false);
        }
        // GetForegroundWindow reports the current foreground HWND or NULL.
        if unsafe { GetForegroundWindow() } as usize == window {
            return Ok(true);
        }
        sleep(Duration::from_millis(50)).await;
    }
    Ok(false)
}

fn request_foreground(pid: u32, process: &OwnedHandle) -> Result<Option<usize>> {
    let mut search = WindowSearch {
        pid,
        first: None,
        multiple: false,
    };
    // EnumWindows calls the callback synchronously with the supplied context.
    checked(unsafe { EnumWindows(Some(find_window), std::ptr::from_mut(&mut search) as LPARAM) })?;
    let Some(window) = search.first else {
        return Ok(None);
    };
    if search.multiple || !process_alive(process) || window_pid(window) != pid {
        return Ok(None);
    }

    // This is the sole terminal window in the verified WezTerm process.
    unsafe {
        if IsWindowVisible(window) == 0 {
            return Ok(None);
        }
        if IsIconic(window) != 0 {
            ShowWindowAsync(window, SW_RESTORE);
        }
        SetForegroundWindow(window);
    }
    Ok(Some(window as usize))
}

struct WindowSearch {
    pid: u32,
    first: Option<HWND>,
    multiple: bool,
}

unsafe extern "system" fn find_window(window: HWND, context: LPARAM) -> BOOL {
    // EnumWindows passes back the exclusive pointer for this synchronous call.
    let search = unsafe { &mut *(context as *mut WindowSearch) };
    // Owned windows include dialogs and cannot establish a terminal mapping.
    if window_pid(window) == search.pid && unsafe { GetWindow(window, GW_OWNER) }.is_null() {
        let mut class = [0; 128];
        // The buffer is writable for the supplied number of UTF-16 code units.
        let len = unsafe { GetClassNameW(window, class.as_mut_ptr(), class.len() as i32) };
        if len > 0
            && class[..len as usize]
                .iter()
                .copied()
                .eq(WINDOW_CLASS.encode_utf16())
        {
            search.multiple |= search.first.replace(window).is_some();
        }
    }
    1
}

fn window_pid(window: HWND) -> u32 {
    let mut pid = 0;
    // The system validates the HWND and writes the corresponding PID.
    unsafe { GetWindowThreadProcessId(window, &mut pid) };
    pid
}
