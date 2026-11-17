use super::Event;
use crate::{notifications::Notice, ui_signal::UiSignal};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tokio::sync::mpsc;

struct Bridge {
    events: mpsc::Sender<Event>,
    signal: UiSignal,
    notices: BTreeMap<uuid::Uuid, Notice>,
}

static BRIDGE: OnceLock<Mutex<Option<Bridge>>> = OnceLock::new();

unsafe extern "C" {
    fn corvid_native_start();
    fn corvid_native_stop();
    fn corvid_native_status(text: *const std::ffi::c_char, attention: bool);
    fn corvid_native_notice(
        high: u64,
        low: u64,
        title: *const std::ffi::c_char,
        body: *const std::ffi::c_char,
        sound: bool,
    );
}

#[unsafe(no_mangle)]
extern "C" fn corvid_native_event(kind: u32, high: u64, low: u64) {
    let Ok(guard) = BRIDGE.get_or_init(|| Mutex::new(None)).lock() else {
        return;
    };
    let Some(bridge) = guard.as_ref() else {
        return;
    };
    let id = uuid::Uuid::from_u128((u128::from(high) << 64) | u128::from(low));
    let event = match kind {
        1 => Event::Open,
        2 => Event::Notifications,
        3 => Event::Pause,
        4 => Event::Quit,
        5 => match bridge.notices.get(&id) { Some(notice) => Event::Notice(notice.clone()), None => return },
        _ => Event::Error("macOS notifications are unavailable. Check System Settings → Notifications and use the installed app bundle.".into()),
    };
    let _ = bridge.events.try_send(event);
    bridge.signal.notify();
}

pub struct Native {
    controller: uuid::Uuid,
    state: PathBuf,
    address: PathBuf,
}

impl Native {
    pub fn start(
        events: mpsc::Sender<Event>,
        signal: UiSignal,
        controller: uuid::Uuid,
        state: &Path,
        address: &Path,
    ) -> Result<Self> {
        *BRIDGE.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(Bridge {
            events,
            signal,
            notices: BTreeMap::new(),
        });
        // The worker starts before GPUI's run loop. AppKit setup is queued, never
        // synchronously dispatched while the foreground thread waits for us.
        unsafe { corvid_native_start() };
        Ok(Self {
            controller,
            state: state.into(),
            address: address.into(),
        })
    }

    pub fn background(&self) -> bool {
        true
    }

    pub fn status(&self, text: &str, attention: bool) {
        if let Ok(text) = CString::new(text) {
            unsafe { corvid_native_status(text.as_ptr(), attention) };
        }
    }

    pub fn startup(&self, enabled: bool) -> Result<()> {
        let home = std::env::var_os("HOME").context("HOME is unavailable")?;
        let directory = PathBuf::from(home).join("Library/LaunchAgents");
        let label = format!("org.corvid.agent-deck.{}", self.controller.simple());
        let path = directory.join(format!("{label}.plist"));
        if !enabled {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            return Ok(());
        }
        let executable = corvid_update::layout::unix_startup_executable(&std::env::current_exe()?);
        let (socket_flag, socket) =
            match corvid_protocol::paths::macos_startup_socket_name(&self.address)? {
                Some(name) => ("--socket-name", name),
                None => ("--socket-dir", self.address.as_os_str()),
            };
        let arguments = [
            executable.to_string_lossy().into_owned(),
            "--background".into(),
            "--state-dir".into(),
            self.state.to_string_lossy().into_owned(),
            socket_flag.into(),
            socket.to_string_lossy().into_owned(),
        ];
        let arguments = arguments
            .iter()
            .map(|value| format!("<string>{}</string>", xml(value)))
            .collect::<String>();
        let contents = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array>{arguments}</array><key>RunAtLoad</key><true/><key>ProcessType</key><string>Interactive</string></dict></plist>\n"
        );
        std::fs::create_dir_all(&directory)?;
        use std::io::Write;
        let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
        temporary.write_all(contents.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(path)?;
        Ok(())
    }

    pub async fn notify(&self, notice: Notice, sound: bool) -> Result<()> {
        let title = CString::new(notice.title.as_str())?;
        let body = CString::new(notice.body.as_str())?;
        let mut guard = BRIDGE.get().unwrap().lock().unwrap();
        let bridge = guard.as_mut().context("notification service stopped")?;
        let id = uuid::Uuid::new_v4();
        bridge.notices.insert(id, notice);
        while bridge.notices.len() > 128 {
            bridge.notices.pop_first();
        }
        unsafe {
            corvid_native_notice(
                (id.as_u128() >> 64) as u64,
                id.as_u128() as u64,
                title.as_ptr(),
                body.as_ptr(),
                sound,
            )
        };
        Ok(())
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        *BRIDGE.get().unwrap().lock().unwrap() = None;
        unsafe { corvid_native_stop() };
    }
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
