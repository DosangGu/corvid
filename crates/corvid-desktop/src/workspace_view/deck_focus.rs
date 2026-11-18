use corvid_presentation::desktop_focus::FocusReply;
use gpui::{Context, Window};

use super::WorkspaceView;

impl WorkspaceView {
    pub(super) fn focus_from_deck(
        &mut self,
        window: &mut Window,
        cx: &Context<Self>,
        reply: FocusReply,
    ) {
        let Some(terminal) = &self.terminal else {
            let _ = reply.send(Err("The selected terminal pane is unavailable.".into()));
            return;
        };
        self.naming = None;
        self.creation = None;
        self.menu_open = false;
        self.switcher_open = false;
        terminal.read(cx).focus(window);
        activate(window, cx, reply);
    }
}

#[cfg(target_os = "linux")]
fn activate(window: &Window, _: &Context<WorkspaceView>, reply: FocusReply) {
    window.activate_window();
    let _ = reply.send(Ok(()));
}

#[cfg(windows)]
fn activate(window: &Window, cx: &Context<WorkspaceView>, reply: FocusReply) {
    use std::time::Duration;

    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, IsIconic, SW_RESTORE, SetForegroundWindow,
        ShowWindow,
    };

    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        let _ = reply.send(Err("Could not access the selected Desktop window.".into()));
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        let _ = reply.send(Err(
            "Could not access the selected Windows Desktop window.".into()
        ));
        return;
    };
    let hwnd = handle.hwnd.get() as usize;
    // Restoring a window can synchronously dispatch resize and activation
    // messages. Run it after releasing the GPUI window and view borrows.
    let executor = cx.background_executor().clone();
    cx.foreground_executor()
        .spawn(async move {
            if reply.is_closed() {
                return;
            }
            let hwnd = hwnd as HWND;
            let granted = unsafe {
                let mut owner = 0;
                GetWindowThreadProcessId(hwnd, &mut owner);
                if owner != std::process::id() {
                    let _ = reply.send(Err("The selected Desktop window closed.".into()));
                    return;
                }
                let granted = GetForegroundWindow() == hwnd || SetForegroundWindow(hwnd) != 0;
                if granted && IsIconic(hwnd) != 0 {
                    ShowWindow(hwnd, SW_RESTORE);
                }
                granted
            };
            if granted {
                for _ in 0..10 {
                    if unsafe { GetForegroundWindow() == hwnd && IsIconic(hwnd) == 0 } {
                        let _ = reply.send(Ok(()));
                        return;
                    }
                    executor.timer(Duration::from_millis(25)).await;
                }
            }
            let _ = reply.send(Err(FOREGROUND_FAILED.into()));
        })
        .detach();
}

#[cfg(any(windows, target_os = "macos"))]
const FOREGROUND_FAILED: &str = "The pane was selected, but the OS did not bring Desktop forward.";

#[cfg(target_os = "macos")]
fn activate(window: &Window, _: &Context<WorkspaceView>, reply: FocusReply) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        let _ = reply.send(Err(FOREGROUND_FAILED.into()));
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        let _ = reply.send(Err(FOREGROUND_FAILED.into()));
        return;
    };
    // AppKit captures a weak reference to the window before this borrow ends.
    unsafe {
        corvid_desktop_activate_window(
            handle.ns_view.as_ptr(),
            Box::into_raw(Box::new(reply)).cast(),
        );
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn corvid_desktop_activate_window(view: *mut std::ffi::c_void, reply: *mut std::ffi::c_void);
}

#[cfg(target_os = "macos")]
#[unsafe(no_mangle)]
extern "C" fn corvid_desktop_activation_pending(reply: *const std::ffi::c_void) -> bool {
    !unsafe { &*reply.cast::<FocusReply>() }.is_closed()
}

#[cfg(target_os = "macos")]
#[unsafe(no_mangle)]
extern "C" fn corvid_desktop_activation_finished(reply: *mut std::ffi::c_void, active: bool) {
    let reply = unsafe { Box::from_raw(reply.cast::<FocusReply>()) };
    let _ = reply.send(if active {
        Ok(())
    } else {
        Err(FOREGROUND_FAILED.into())
    });
}
