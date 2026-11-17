use gpui::{Context, Window};
use tokio::sync::oneshot;

use super::WorkspaceView;

impl WorkspaceView {
    pub(super) fn focus_from_deck(
        &mut self,
        window: &mut Window,
        cx: &Context<Self>,
        reply: oneshot::Sender<Result<(), String>>,
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

#[cfg(not(windows))]
fn activate(
    window: &Window,
    _: &Context<WorkspaceView>,
    reply: oneshot::Sender<Result<(), String>>,
) {
    window.activate_window();
    let _ = reply.send(Ok(()));
}

#[cfg(windows)]
fn activate(
    window: &Window,
    cx: &Context<WorkspaceView>,
    reply: oneshot::Sender<Result<(), String>>,
) {
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
    cx.foreground_executor()
        .spawn(async move {
            if reply.is_closed() {
                return;
            }
            let hwnd = hwnd as HWND;
            let result = unsafe {
                let mut owner = 0;
                GetWindowThreadProcessId(hwnd, &mut owner);
                if owner != std::process::id() {
                    Err("The selected Desktop window closed.".into())
                } else {
                    if IsIconic(hwnd) != 0 {
                        ShowWindow(hwnd, SW_RESTORE);
                    }
                    // Deck grants activation to the PID of the registered
                    // Desktop peer when the user releases the card.
                    if GetForegroundWindow() == hwnd || SetForegroundWindow(hwnd) != 0 {
                        Ok(())
                    } else {
                        Err(
                            "The pane was selected, but Windows did not bring Desktop forward."
                                .into(),
                        )
                    }
                }
            };
            let _ = reply.send(result);
        })
        .detach();
}
