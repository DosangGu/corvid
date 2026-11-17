use super::Callbacks;
use anyhow::{Context, Result};
use corvid_agent_deck_core::native::{
    Availability, Capabilities, Event, NativeError, NativeServices, Status,
};
use corvid_agent_deck_core::notifications::Notice;
use std::collections::VecDeque;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::timeout;
use uuid::Uuid;

struct Pending {
    id: Uuid,
    notice: Notice,
    reply: oneshot::Sender<Result<(), NativeError>>,
}

struct Bridge {
    callbacks: Callbacks,
    active: VecDeque<(Uuid, Notice)>,
    pending: Option<Pending>,
}

static BRIDGE: Mutex<Option<Bridge>> = Mutex::new(None);

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

fn notifications_unavailable() -> NativeError {
    NativeError::Rejected(
        "macOS notifications are unavailable. Check System Settings → Notifications and use the installed app bundle.".into(),
    )
}

#[unsafe(no_mangle)]
extern "C" fn corvid_native_ready(status_icon: bool, notifications: bool) {
    let Ok(guard) = BRIDGE.lock() else {
        return;
    };
    let Some(bridge) = guard.as_ref() else {
        return;
    };
    let availability = |ready| {
        if ready {
            Availability::Ready
        } else {
            Availability::Unavailable
        }
    };
    bridge.callbacks.capabilities.send_replace(Capabilities {
        status_icon: availability(status_icon),
        notifications: availability(notifications),
        activation: availability(notifications),
        sound: availability(notifications),
        login_startup: Availability::Ready,
    });
    if !status_icon {
        bridge.callbacks.issue(NativeError::Rejected(
            "Could not add the macOS menu bar icon".into(),
        ));
    } else if !notifications {
        bridge.callbacks.issue(notifications_unavailable());
    }
}

fn request_id(high: u64, low: u64) -> Uuid {
    Uuid::from_u128((u128::from(high) << 64) | u128::from(low))
}

#[unsafe(no_mangle)]
extern "C" fn corvid_native_submitted(high: u64, low: u64, accepted: bool) {
    let Ok(mut guard) = BRIDGE.lock() else {
        return;
    };
    let Some(bridge) = guard.as_mut() else {
        return;
    };
    let id = request_id(high, low);
    let Some(pending) = bridge.pending.take_if(|pending| pending.id == id) else {
        return;
    };
    let result = if accepted {
        bridge.active.push_back((id, pending.notice));
        if bridge.active.len() > 128 {
            bridge.active.pop_front();
        }
        Ok(())
    } else {
        Err(notifications_unavailable())
    };
    let _ = pending.reply.send(result);
}

#[unsafe(no_mangle)]
extern "C" fn corvid_native_event(kind: u32, high: u64, low: u64) {
    let Ok(guard) = BRIDGE.lock() else {
        return;
    };
    let Some(bridge) = guard.as_ref() else {
        return;
    };
    let callbacks = &bridge.callbacks;
    let event = match kind {
        1 => return callbacks.signal.open(),
        2 => Event::OpenNotifications,
        3 => Event::TogglePause,
        4 => return callbacks.signal.quit(),
        5 => {
            let id = request_id(high, low);
            let Some((_, notice)) = bridge.active.iter().find(|(key, _)| *key == id) else {
                return;
            };
            Event::Activate(notice.clone())
        }
        7 => return callbacks.signal.toggle(),
        _ => return,
    };
    let _ = callbacks.event(event);
}

pub(crate) struct Macos {
    login: LoginRegistration,
}

struct LoginRegistration {
    controller: Uuid,
    state: PathBuf,
    address: PathBuf,
}

pub(crate) fn start(
    callbacks: Callbacks,
    controller: Uuid,
    state: &Path,
    address: &Path,
) -> Result<Macos> {
    callbacks.capabilities.send_replace(Capabilities {
        status_icon: Availability::Starting,
        notifications: Availability::Starting,
        activation: Availability::Starting,
        sound: Availability::Starting,
        login_startup: Availability::Ready,
    });
    let mut bridge = BRIDGE
        .lock()
        .map_err(|_| anyhow::anyhow!("macOS native bridge unavailable"))?;
    anyhow::ensure!(bridge.is_none(), "macOS native bridge already running");
    *bridge = Some(Bridge {
        callbacks,
        active: VecDeque::new(),
        pending: None,
    });
    // GPUI starts its event loop after the background worker acknowledges
    // startup. Report native readiness from the queued AppKit setup.
    unsafe { corvid_native_start() };
    Ok(Macos {
        login: LoginRegistration {
            controller,
            state: state.into(),
            address: address.into(),
        },
    })
}

impl LoginRegistration {
    fn startup(&self, enabled: bool) -> Result<()> {
        let home = std::env::var_os("HOME").context("HOME is unavailable")?;
        self.startup_in(enabled, &PathBuf::from(home).join("Library/LaunchAgents"))
    }

    fn startup_in(&self, enabled: bool, directory: &Path) -> Result<()> {
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
        std::fs::create_dir_all(directory)?;
        use std::io::Write;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        temporary.write_all(contents.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(path)?;
        Ok(())
    }
}

impl NativeServices for Macos {
    fn update_status(&mut self, status: Status) -> Result<(), NativeError> {
        let text =
            CString::new(status.text).map_err(|error| NativeError::Rejected(error.to_string()))?;
        let bridge = BRIDGE.lock().map_err(|_| NativeError::Disconnected)?;
        let bridge = bridge.as_ref().ok_or(NativeError::Disconnected)?;
        bridge.callbacks.capabilities.borrow().status_icon.check()?;
        unsafe { corvid_native_status(text.as_ptr(), status.attention) };
        Ok(())
    }

    async fn set_login_startup(&mut self, enabled: bool) -> Result<(), NativeError> {
        self.login
            .startup(enabled)
            .map_err(|error| NativeError::Rejected(error.to_string()))
    }

    async fn notify(&mut self, notice: Notice, sound: bool) -> Result<(), NativeError> {
        let title = CString::new(notice.title.as_str())
            .map_err(|error| NativeError::Rejected(error.to_string()))?;
        let body = CString::new(notice.body.as_str())
            .map_err(|error| NativeError::Rejected(error.to_string()))?;
        let (reply, result) = oneshot::channel();
        {
            let mut guard = BRIDGE.lock().map_err(|_| NativeError::Disconnected)?;
            let bridge = guard.as_mut().ok_or(NativeError::Disconnected)?;
            let capabilities = *bridge.callbacks.capabilities.borrow();
            capabilities.notifications.check()?;
            if sound {
                capabilities.sound.check()?;
            }
            if bridge.pending.is_some() {
                return Err(NativeError::QueueFull);
            }
            let id = Uuid::new_v4();
            bridge.pending = Some(Pending { id, notice, reply });
            unsafe {
                corvid_native_notice(
                    (id.as_u128() >> 64) as u64,
                    id.as_u128() as u64,
                    title.as_ptr(),
                    body.as_ptr(),
                    sound,
                )
            };
        }
        // Authorization can require user interaction. Keep one request in
        // flight until the OS answers, even if its caller stops waiting.
        timeout(Duration::from_secs(5), result)
            .await
            .map_err(|_| NativeError::TimedOut)?
            .map_err(|_| NativeError::Disconnected)?
    }
}

fn stop() {
    if let Ok(mut guard) = BRIDGE.lock() {
        *guard = None;
    }
    unsafe { corvid_native_stop() };
}

pub(crate) fn shutdown_on_main() {
    stop();
}

impl Drop for Macos {
    fn drop(&mut self) {
        stop();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_startup_writes_and_removes_a_launch_agent() -> Result<()> {
        let root = tempfile::tempdir()?;
        let directory = root.path().join("LaunchAgents");
        let registration = LoginRegistration {
            controller: Uuid::new_v4(),
            state: root.path().join("State & Data"),
            address: root.path().join("companion"),
        };
        registration.startup_in(true, &directory)?;
        let path = directory.join(format!(
            "org.corvid.agent-deck.{}.plist",
            registration.controller.simple()
        ));
        let plist = std::fs::read_to_string(&path)?;
        assert!(plist.contains("--background"));
        assert!(plist.contains("State &amp; Data"));
        assert!(
            std::process::Command::new("plutil")
                .arg("-lint")
                .arg(&path)
                .status()?
                .success()
        );
        registration.startup_in(false, &directory)?;
        assert!(!path.exists());
        Ok(())
    }
}
