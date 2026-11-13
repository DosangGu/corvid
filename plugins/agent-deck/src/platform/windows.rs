mod toast;
pub use toast::identify;

use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;

use anyhow::{Context, Result, ensure};
use eframe::egui;
use tokio::sync::mpsc;
use uuid::Uuid;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::Event;
use crate::notifications::Notice;

const TRAY: u32 = WM_APP + 1;
const COMMAND: u32 = WM_APP + 2;
const KEY_SELECTED: u32 = NIN_SELECT | NINF_KEY;

enum Command {
    Notify(Notice, bool),
    Status(String, bool),
    Startup(bool, SyncSender<Result<(), String>>),
}

pub struct Native {
    window: usize,
    commands: SyncSender<Command>,
    worker: Option<JoinHandle<()>>,
}

impl Native {
    pub fn start(
        events: mpsc::Sender<Event>,
        ctx: egui::Context,
        controller: Uuid,
        state: &Path,
        address: &str,
    ) -> Result<Self> {
        let (commands, receiver) = sync_channel(32);
        let (ready, started) = sync_channel(1);
        let state = state.to_owned();
        let address = address.to_owned();
        let worker = std::thread::Builder::new()
            .name("corvid-agent-deck-tray".into())
            .spawn(move || {
                let result = run(
                    receiver,
                    events.clone(),
                    ctx.clone(),
                    controller,
                    state,
                    address,
                    ready.clone(),
                );
                if let Err(error) = result {
                    let message = error.to_string();
                    let _ = ready.try_send(Err(message.clone()));
                    let _ = events.try_send(Event::Error(message));
                    ctx.request_repaint();
                }
            })?;
        let window = started
            .recv_timeout(std::time::Duration::from_secs(5))
            .context("Agent Deck tray startup timed out")?
            .map_err(anyhow::Error::msg)?;
        Ok(Self {
            window,
            commands,
            worker: Some(worker),
        })
    }

    pub fn background(&self) -> bool {
        true
    }
    fn send(&self, command: Command) -> Result<()> {
        self.commands
            .try_send(command)
            .context("Agent Deck OS queue is full or unavailable")?;
        ensure!(
            unsafe { PostMessageW(self.window as HWND, COMMAND, 0, 0) } != 0,
            "Agent Deck tray window is unavailable"
        );
        Ok(())
    }
    pub fn status(&self, text: &str, attention: bool) {
        let _ = self.send(Command::Status(text.to_owned(), attention));
    }
    pub fn startup(&self, enabled: bool) -> Result<()> {
        let (sent, result) = sync_channel(1);
        self.send(Command::Startup(enabled, sent))?;
        result
            .recv_timeout(std::time::Duration::from_secs(3))
            .context("Windows startup registration timed out")?
            .map_err(anyhow::Error::msg)
    }
    pub async fn notify(&self, notice: Notice, sound: bool) -> Result<()> {
        self.send(Command::Notify(notice, sound))
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        // WM_CLOSE does not depend on free capacity in the bounded command queue.
        unsafe {
            PostMessageW(self.window as HWND, WM_CLOSE, 0, 0);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct State {
    commands: Receiver<Command>,
    events: mpsc::Sender<Event>,
    ctx: egui::Context,
    tray: NOTIFYICONDATAW,
    restart: u32,
    toast: Option<toast::Sink>,
}

fn run(
    commands: Receiver<Command>,
    events: mpsc::Sender<Event>,
    ctx: egui::Context,
    controller: Uuid,
    state: std::path::PathBuf,
    address: String,
    ready: SyncSender<Result<usize, String>>,
) -> Result<()> {
    let apartment = toast::Apartment::new()?;
    let toast = match toast::Sink::new(controller, &state, &address, events.clone(), ctx.clone()) {
        Ok(sink) => Some(sink),
        Err(error) => {
            let _ = events.try_send(Event::Error(format!(
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
        commands,
        events,
        ctx,
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
    if ready.send(Ok(window as usize)).is_err() {
        unsafe {
            DestroyWindow(window);
        }
    }
    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    unsafe {
        Shell_NotifyIconW(NIM_DELETE, &state.tray);
        UnregisterClassW(class.as_ptr(), module);
    }
    drop(state);
    drop(apartment);
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
    fn event(&self, event: Event) {
        dispatch_event(&self.events, &self.ctx, event);
    }
    fn commands(&mut self) {
        while let Ok(command) = self.commands.try_recv() {
            let result = match command {
                Command::Notify(notice, sound) => self
                    .toast
                    .as_mut()
                    .context("Windows notifications are unavailable")
                    .and_then(|sink| sink.show(notice, sound)),
                Command::Status(text, attention) => {
                    set_text(&mut self.tray.szTip, &text);
                    self.tray.hIcon = unsafe {
                        LoadIconW(
                            GetModuleHandleW(null()),
                            icon_resource(if attention { 3 } else { 2 }),
                        )
                    };
                    unsafe {
                        Shell_NotifyIconW(NIM_MODIFY, &self.tray);
                    }
                    Ok(())
                }
                Command::Startup(enabled, reply) => {
                    let result = self
                        .toast
                        .as_ref()
                        .context("Windows app registration is unavailable")
                        .and_then(|sink| sink.startup(enabled));
                    let _ = reply.send(result.map_err(|error| error.to_string()));
                    Ok(())
                }
            };
            if let Err(error) = result {
                self.event(Event::Error(error.to_string()));
            }
        }
    }
    fn menu(&self) {
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
            SetForegroundWindow(self.tray.hWnd);
            let selected = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY,
                point.x,
                point.y,
                0,
                self.tray.hWnd,
                null(),
            );
            DestroyMenu(menu);
            PostMessageW(self.tray.hWnd, WM_NULL, 0, 0);
            match selected {
                1 => self.event(Event::Open),
                2 => self.event(Event::Notifications),
                3 => self.event(Event::Pause),
                4 => self.event(Event::Quit),
                _ => {}
            }
        }
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
            state.install();
            return 0;
        }
        match message {
            COMMAND => {
                state.commands();
                return 0;
            }
            TRAY => {
                match (lparam as u32) & 0xffff {
                    WM_LBUTTONUP | NIN_SELECT | KEY_SELECTED => state.event(Event::Open),
                    WM_CONTEXTMENU | WM_RBUTTONUP => state.menu(),
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
                    PostQuitMessage(0);
                }
                return 0;
            }
            _ => {}
        }
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

fn dispatch_event(events: &mpsc::Sender<Event>, ctx: &egui::Context, event: Event) {
    if matches!(event, Event::Quit) {
        // Quit must reach the UI even while the background worker is busy or
        // its bounded event queue is full.
        crate::window::request_quit(ctx);
        return;
    }
    let _ = events.try_send(event);
    ctx.request_repaint();
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
        let ctx = egui::Context::default();
        let (events, receiver) = mpsc::channel(1);
        events.try_send(Event::Open).unwrap();
        dispatch_event(&events, &ctx, Event::Quit);
        assert!(crate::window::quit_requested(&ctx));

        let ctx = egui::Context::default();
        drop(receiver);
        dispatch_event(&events, &ctx, Event::Quit);
        assert!(crate::window::quit_requested(&ctx));
        let output = ctx.run(egui::RawInput::default(), |_| {});
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&egui::ViewportCommand::Close)
        );
    }
}
