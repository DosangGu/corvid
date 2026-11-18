use corvid_client::ControlEndpoint;
use corvid_presentation::desktop_focus::{FocusReply, FocusRequest};
use corvid_protocol::wire::{PresentedRunKey, ViewSelection};
use gpui::{Context, Window};

use super::WorkspaceView;
use crate::terminal_session::{AttachTarget, Destination, Session, Ui};

impl WorkspaceView {
    pub(crate) fn deck_focus_rank(&self, run: &PresentedRunKey) -> u8 {
        let Some(ui) = self
            .session
            .as_ref()
            .map(Session::ui)
            .filter(|ui| matching_host(ui, run))
        else {
            return 0;
        };
        match (ui.selected.as_ref(), agent_view(&ui, run)) {
            (Some(selected), Some(target)) if selected == target => 3,
            (Some(selected), Some(target)) if selected.workspace_id == target.workspace_id => 2,
            _ => 1,
        }
    }

    pub(crate) fn route_deck_focus(
        &mut self,
        endpoint: ControlEndpoint,
        FocusRequest { run, mut result }: FocusRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.connecting(self.session.as_ref().map(Session::ui).as_ref()) {
            let _ = result.send(Err("Desktop is changing views. Try again.".into()));
            return;
        }
        if self.deck_focus_rank(&run) >= 2
            && let Some(session) = &self.session
        {
            session.focus_agent(FocusRequest { run, result });
            return;
        }
        let destination = match Destination::from_endpoint(&endpoint) {
            Ok(destination) => destination,
            Err(_) => {
                let _ = result.send(Err(
                    "The selected host is unavailable on this platform.".into()
                ));
                return;
            }
        };
        let settings = self.agent_deck.clone();
        let store = self.store.clone();
        let lifetime = self.lifetime.clone();
        self.host_switch = Some(cx.spawn_in(window, async move |view, cx| {
            // A second click can arrive while the previous writer is still retiring.
            while !lifetime.retirement_done() && !result.is_closed() {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(25))
                    .await;
            }
            let start = if result.is_closed() {
                None
            } else {
                Some(Session::start(
                    destination.clone(),
                    settings,
                    store,
                    AttachTarget::Agent { endpoint, run },
                    false,
                    lifetime.parked(),
                ))
            };
            let (session, job, prompts) = match start {
                Some(Ok(connection)) => connection,
                start => {
                    let _ = view.update_in(cx, |view, _, cx| {
                        view.host_switch = None;
                        cx.notify();
                    });
                    if start.is_some() {
                        let _ = result
                            .send(Err("Could not start the agent's Desktop connection.".into()));
                    }
                    return;
                }
            };
            let mut changes = session.changes();
            let outcome = loop {
                let ui = session.ui();
                if ui.can_write() {
                    break Some(Ok((session, job, prompts)));
                }
                if !ui.changing {
                    break Some(Err(ui.message));
                }
                tokio::select! {
                    _ = result.closed() => break None,
                    change = changes.changed() => {
                        if change.is_err() {
                            break Some(Err("The selected terminal pane is unavailable.".into()));
                        }
                    }
                }
            };
            let _ = view.update_in(cx, |view, window, cx| {
                view.host_switch = None;
                match outcome {
                    Some(Ok(connection)) if !result.is_closed() => {
                        connection.0.commit_navigation();
                        view.show_connection(destination, connection, window, cx);
                        view.sync(window, cx);
                        view.focus_from_deck(window, cx, result);
                    }
                    Some(Err(error)) => {
                        let fault = corvid_protocol::ProtocolFault::new("FocusUnavailable", &error);
                        let _ = result.send(Err(fault.into()));
                    }
                    _ => {}
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

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

fn matching_host(ui: &Ui, run: &PresentedRunKey) -> bool {
    ui.connected
        && ui.host_id.as_deref() == Some(run.host_id.as_str())
        && ui.epoch.as_deref() == Some(run.remote_epoch.as_str())
}

fn agent_view<'a>(ui: &'a Ui, run: &PresentedRunKey) -> Option<&'a ViewSelection> {
    let inventory = ui.inventory.as_ref().filter(|_| ui.inventory_ready)?;
    inventory
        .agents
        .iter()
        .find(|agent| agent.agent_run_id == run.agent_run_id && agent.terminal_available)?
        .location
        .as_ref()
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
