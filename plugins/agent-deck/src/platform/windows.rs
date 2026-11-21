mod toast;
pub use toast::identify;

use std::collections::VecDeque;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_async::oneshot;
use tokio::time::timeout;
use uuid::Uuid;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::Callbacks;
use corvid_agent_deck_core::native::{
    Availability, Capabilities, Event, NativeError, NativeServices, Status,
};
use corvid_agent_deck_core::notifications::Notice;

const TRAY: u32 = WM_APP + 1;
const COMMAND: u32 = WM_APP + 2;
const KEY_SELECTED: u32 = NIN_SELECT | NINF_KEY;

type Reply = oneshot::Sender<Result<(), NativeError>>;

enum Command {
    Notify(Notice, bool, Reply),
    Startup(bool, Reply),
}

#[derive(Default)]
struct Pending {
    commands: VecDeque<Command>,
    status: Option<Status>,
}

pub(crate) struct Windows {
    window: usize,
    pending: Arc<Mutex<Pending>>,
    callbacks: Callbacks,
    worker: Option<JoinHandle<()>>,
}

pub(crate) fn start(
    callbacks: Callbacks,
    controller: Uuid,
    state: &Path,
    address: &str,
) -> Result<Windows> {
    callbacks.capabilities.send_replace(Capabilities {
        status_icon: Availability::Starting,
        notifications: Availability::Starting,
        activation: Availability::Starting,
        sound: Availability::Starting,
        login_startup: Availability::Starting,
    });
    let pending = Arc::new(Mutex::new(Pending::default()));
    let worker_pending = pending.clone();
    let worker_callbacks = callbacks.clone();
    let (ready, started) = sync_channel(1);
    let state = state.to_owned();
    let address = address.to_owned();
    let worker = std::thread::Builder::new()
        .name("corvid-agent-deck-tray".into())
        .spawn(move || {
            let result = run(
                worker_pending,
                worker_callbacks.clone(),
                controller,
                state,
                address,
                &ready,
            );
            worker_callbacks
                .capabilities
                .send_modify(|state| *state = state.stopped());
            if let Err(error) = result {
                let message = error.to_string();
                let _ = ready.try_send(Err(message.clone()));
                worker_callbacks.issue(NativeError::Rejected(message));
            }
        })?;
    let window = started
        .recv_timeout(Duration::from_secs(5))
        .context("Agent Deck tray startup timed out")?
        .map_err(anyhow::Error::msg)?;
    Ok(Windows {
        window,
        pending,
        callbacks,
        worker: Some(worker),
    })
}

impl Windows {
    fn post(&self, pending: &Pending) -> Result<(), NativeError> {
        // A posted message covers the pending batch. Hold the lock until the
        // update is installed, so a failed wake never leaves a queued command.
        if pending.commands.is_empty()
            && pending.status.is_none()
            && unsafe { PostMessageW(self.window as HWND, COMMAND, 0, 0) } == 0
        {
            return Err(NativeError::Disconnected);
        }
        Ok(())
    }

    async fn request(&self, command: impl FnOnce(Reply) -> Command) -> Result<(), NativeError> {
        let (reply, result) = oneshot::channel();
        {
            let mut pending = self.pending.lock().map_err(|_| NativeError::Disconnected)?;
            if pending.commands.len() >= 32 {
                return Err(NativeError::QueueFull);
            }
            self.post(&pending)?;
            pending.commands.push_back(command(reply));
        }
        timeout(Duration::from_secs(3), result)
            .await
            .map_err(|_| NativeError::TimedOut)?
            .map_err(|_| NativeError::Disconnected)?
    }
}

impl NativeServices for Windows {
    fn update_status(&mut self, status: Status) -> Result<(), NativeError> {
        self.callbacks.capabilities.borrow().status_icon.check()?;
        let mut pending = self.pending.lock().map_err(|_| NativeError::Disconnected)?;
        self.post(&pending)?;
        pending.status = Some(status);
        Ok(())
    }

    async fn notify(&mut self, notice: Notice, sound: bool) -> Result<(), NativeError> {
        let capabilities = *self.callbacks.capabilities.borrow();
        capabilities.notifications.check()?;
        if sound {
            capabilities.sound.check()?;
        }
        self.request(|reply| Command::Notify(notice, sound, reply))
            .await
    }

    async fn set_login_startup(&mut self, enabled: bool) -> Result<(), NativeError> {
        self.callbacks.capabilities.borrow().login_startup.check()?;
        self.request(|reply| Command::Startup(enabled, reply)).await
    }
}

impl Drop for Windows {
    fn drop(&mut self) {
        // WM_CLOSE does not depend on free space in Pending.
        unsafe {
            if PostMessageW(self.window as HWND, WM_CLOSE, 0, 0) == 0 {
                SendNotifyMessageW(self.window as HWND, WM_CLOSE, 0, 0);
            }
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct State {
    pending: Arc<Mutex<Pending>>,
    callbacks: Callbacks,
    tray: NOTIFYICONDATAW,
    restart: u32,
    toast: Option<toast::Sink>,
}

fn run(
    pending: Arc<Mutex<Pending>>,
    callbacks: Callbacks,
    controller: Uuid,
    state: std::path::PathBuf,
    address: String,
    ready: &SyncSender<Result<usize, String>>,
) -> Result<()> {
    let apartment = toast::Apartment::new()?;
    let toast = match toast::Sink::new(controller, &state, &address, callbacks.clone()) {
        Ok(sink) => Some(sink),
        Err(error) => {
            callbacks.issue(NativeError::Rejected(format!(
                "Windows notifications unavailable: {error}"
            )));
            None
        }
    };
    let class = wide("CorvidAgentDeckTray");
    let module = unsafe { GetModuleHandleW(null()) };
    let wc = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: module,
        lpszClassName: class.as_ptr(),
        ..Default::default()
    };
    ensure!(
        unsafe { RegisterClassW(&wc) } != 0,
        "cannot register Agent Deck tray window"
    );
    let mut state = Box::new(State {
        pending,
        callbacks,
        tray: NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: TRAY,
            hIcon: unsafe { LoadIconW(module, icon_resource(2)) },
            ..Default::default()
        },
        restart: unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) },
        toast,
    });
    let window = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            module,
            (&mut *state as *mut State).cast(),
        )
    };
    ensure!(!window.is_null(), "cannot create Agent Deck tray window");
    state.tray.hWnd = window;
    set_text(&mut state.tray.szTip, "Corvid Agent Deck — connecting");
    if !state.install() {
        unsafe {
            DestroyWindow(window);
            UnregisterClassW(class.as_ptr(), module);
        }
        anyhow::bail!("cannot add Agent Deck to the system tray");
    }
    let toast_state = if state.toast.is_some() {
        Availability::Ready
    } else {
        Availability::Unavailable
    };
    state.callbacks.capabilities.send_replace(Capabilities {
        status_icon: Availability::Ready,
        notifications: toast_state,
        activation: toast_state,
        sound: toast_state,
        login_startup: toast_state,
    });
    if ready.send(Ok(window as usize)).is_err() {
        unsafe {
            DestroyWindow(window);
        }
    }
    let mut message = MSG::default();
    let outcome = loop {
        let result = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
        if result <= 0 {
            break result;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    };
    unsafe {
        if IsWindow(window) != 0 {
            DestroyWindow(window);
        }
        UnregisterClassW(class.as_ptr(), module);
    }
    state
        .callbacks
        .capabilities
        .send_modify(|capabilities| *capabilities = capabilities.stopped());
    if let Ok(mut pending) = state.pending.lock() {
        *pending = Pending::default();
    }
    drop(state);
    drop(apartment);
    ensure!(outcome == 0, "Windows tray message loop failed");
    Ok(())
}

impl State {
    fn install(&mut self) -> bool {
        if unsafe { Shell_NotifyIconW(NIM_ADD, &self.tray) } == 0 {
            return false;
        }
        self.tray.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        unsafe {
            Shell_NotifyIconW(NIM_SETVERSION, &self.tray);
        }
        true
    }
    fn commands(&mut self) {
        let Ok(mut pending) = self.pending.lock() else {
            self.callbacks.issue(NativeError::Disconnected);
            return;
        };
        let batch = std::mem::take(&mut *pending);
        drop(pending);
        if let Some(status) = batch.status {
            self.update_status(status);
        }
        for command in batch.commands {
            self.apply(command);
        }
    }

    fn update_status(&mut self, status: Status) {
        set_text(&mut self.tray.szTip, &status.text);
        self.tray.hIcon = unsafe {
            LoadIconW(
                GetModuleHandleW(null()),
                icon_resource(if status.attention { 3 } else { 2 }),
            )
        };
        if unsafe { Shell_NotifyIconW(NIM_MODIFY, &self.tray) } == 0 {
            self.callbacks.issue(NativeError::Rejected(
                "Could not update Windows tray icon".into(),
            ));
        }
    }

    fn apply(&mut self, command: Command) {
        let (reply, result) = match command {
            Command::Notify(notice, sound, reply) if !reply.is_closed() => {
                let result = self
                    .toast
                    .as_mut()
                    .context("Windows notifications are unavailable")
                    .and_then(|sink| sink.show(notice, sound));
                (reply, result)
            }
            Command::Startup(enabled, reply) if !reply.is_closed() => {
                let result = self
                    .toast
                    .as_ref()
                    .context("Windows app registration is unavailable")
                    .and_then(|sink| sink.startup(enabled));
                (reply, result)
            }
            _ => return,
        };
        let _ = reply.send(result.map_err(|error| NativeError::Rejected(error.to_string())));
    }
}

fn menu(window: HWND) {
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        for (id, caption) in [
            (1, "Open Corvid Agent Deck"),
            (2, "Notifications and settings"),
            (3, "Pause / resume notifications"),
            (4, "Quit Corvid Agent Deck"),
        ] {
            AppendMenuW(menu, MF_STRING, id, wide(caption).as_ptr());
        }
        let mut point = POINT::default();
        GetCursorPos(&mut point);
        SetForegroundWindow(window);
        TrackPopupMenu(menu, TPM_LEFTBUTTON, point.x, point.y, 0, window, null());
        DestroyMenu(menu);
        PostMessageW(window, WM_NULL, 0, 0);
    }
}

fn menu_action(selected: u32, callbacks: &Callbacks) {
    match selected {
        1 => callbacks.signal.reopen_from_tray(),
        2 => {
            let _ = callbacks.event(Event::OpenNotifications);
        }
        3 => {
            let _ = callbacks.event(Event::TogglePause);
        }
        4 => callbacks.signal.quit(),
        _ => {}
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        unsafe {
            SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
        }
    }
    let pointer = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut State;
    if !pointer.is_null() {
        let state = unsafe { &mut *pointer };
        if message == state.restart && state.restart != 0 {
            let installed = state.install();
            state.callbacks.capabilities.send_modify(|capabilities| {
                capabilities.status_icon = if installed {
                    Availability::Ready
                } else {
                    Availability::Unavailable
                };
            });
            if !installed {
                state.callbacks.issue(NativeError::Rejected(
                    "Could not restore Windows tray icon".into(),
                ));
            }
            return 0;
        }
        match message {
            COMMAND => {
                state.commands();
                return 0;
            }
            WM_COMMAND if lparam == 0 && wparam <= u16::MAX as usize => {
                menu_action(wparam as u32, &state.callbacks);
                return 0;
            }
            TRAY => {
                match (lparam as u32) & 0xffff {
                    WM_LBUTTONUP | NIN_SELECT | KEY_SELECTED => {
                        state.callbacks.signal.reopen_from_tray();
                    }
                    WM_CONTEXTMENU | WM_RBUTTONUP => {
                        menu(window);
                    }
                    _ => {}
                }
                return 0;
            }
            WM_CLOSE => {
                unsafe {
                    DestroyWindow(window);
                }
                return 0;
            }
            WM_DESTROY => {
                unsafe {
                    Shell_NotifyIconW(NIM_DELETE, &state.tray);
                    PostQuitMessage(0);
                }
                return 0;
            }
            _ => {}
        }
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn icon_resource(id: u16) -> *const u16 {
    // Win32 MAKEINTRESOURCE encodes a resource ID, not a dereferenceable pointer.
    id as usize as *const u16
}
fn set_text<const N: usize>(buffer: &mut [u16; N], value: &str) {
    buffer.fill(0);
    for (target, value) in buffer[..N - 1].iter_mut().zip(value.encode_utf16()) {
        *target = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_bypasses_a_full_or_disconnected_background_queue() {
        let (events, receiver) = corvid_async::mpsc::channel(1);
        let mut callbacks = Callbacks {
            events,
            issues: corvid_async::watch::channel(None).0,
            capabilities: corvid_async::watch::channel(Default::default()).0,
            signal: crate::ui_signal::UiSignal::default(),
        };
        callbacks.event(Event::OpenNotifications).unwrap();
        menu_action(4, &callbacks);
        assert!(callbacks.signal.quitting());

        callbacks.signal = Default::default();
        drop(receiver);
        menu_action(4, &callbacks);
        assert!(callbacks.signal.quitting());
    }
}
