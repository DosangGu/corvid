use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};

use crate::ui_signal::UiSignal;
use anyhow::{Context, Result, ensure};
use tokio::sync::mpsc;
use uuid::Uuid;
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{
    NotificationSetting, ToastNotification, ToastNotificationManager, ToastNotifier,
};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, IPersistFile,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::System::WinRT::{RO_INIT_SINGLETHREADED, RoInitialize, RoUninitialize};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{
    FOLDERID_Programs, FOLDERID_Startup, IShellLinkW, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
    SHStrDupW, ShellLink,
};
use windows::core::{GUID, HSTRING, Interface, PCWSTR};

use crate::notifications::Notice;
use crate::platform::Event;

pub struct Apartment;
impl Apartment {
    pub fn new() -> Result<Self> {
        unsafe {
            RoInitialize(RO_INIT_SINGLETHREADED)?;
        }
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}

pub struct Sink {
    id: HSTRING,
    controller: Uuid,
    notifier: ToastNotifier,
    active: VecDeque<(ToastNotification, i64)>,
    events: mpsc::Sender<Event>,
    ctx: UiSignal,
    arguments: String,
}

impl Sink {
    pub fn new(
        controller: Uuid,
        state: &Path,
        address: &str,
        events: mpsc::Sender<Event>,
        ctx: UiSignal,
    ) -> Result<Self> {
        let id = identity(controller);
        let arguments = format!(
            "--state-dir {} --pipe-instance {}",
            quote(&state.to_string_lossy()),
            quote(address)
        );
        let shortcut =
            shortcut_path(&FOLDERID_Programs, controller).context("locating Windows Start Menu")?;
        save_shortcut(&shortcut, &arguments, &id).context("registering Agent Deck shortcut")?;
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&id)
            .context("creating Windows toast notifier")?;
        Ok(Self {
            id,
            controller,
            notifier,
            active: VecDeque::new(),
            events,
            ctx,
            arguments,
        })
    }

    pub fn startup(&self, enabled: bool) -> Result<()> {
        let path = shortcut_path(&FOLDERID_Startup, self.controller)?;
        if enabled {
            save_shortcut(&path, &format!("{} --background", self.arguments), &self.id)
        } else {
            match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            }
        }
    }

    pub fn show(&mut self, notice: Notice, sound: bool) -> Result<()> {
        match self.notifier.Setting() {
            Ok(setting) => ensure!(
                setting == NotificationSetting::Enabled,
                "Windows notification settings currently disable Agent Deck notifications"
            ),
            // An unpackaged sender can lack settings until its first delivery.
            // The OS still applies user/system notification policy in Show.
            Err(error) if error.code().0 as u32 == 0x8007_0490 => {}
            Err(error) => return Err(error).context("reading Windows notification settings"),
        }
        let audio = if sound {
            ""
        } else {
            "<audio silent=\"true\"/>"
        };
        let xml = format!(
            "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual>{audio}</toast>",
            escape(&notice.title),
            escape(&notice.body)
        );
        let document = XmlDocument::new()?;
        document.LoadXml(&HSTRING::from(xml))?;
        let toast = ToastNotification::CreateToastNotification(&document)?;
        toast.SetTag(&HSTRING::from(
            &notice.key.attention.simple().to_string()[..16],
        ))?;
        toast.SetGroup(&HSTRING::from("agent-deck"))?;
        let events = self.events.clone();
        let ctx = self.ctx.clone();
        let token = toast.Activated(&TypedEventHandler::new(move |_, _| {
            let _ = events.try_send(if notice.summary {
                Event::Notifications
            } else {
                Event::Notice(notice.clone())
            });
            crate::parent::show(&ctx);
            Ok(())
        }))?;
        self.notifier
            .Show(&toast)
            .context("delivering Windows toast")?;
        self.active.push_back((toast, token));
        while self.active.len() > 64 {
            if let Some((toast, token)) = self.active.pop_front() {
                let _ = self.notifier.Hide(&toast);
                let _ = toast.RemoveActivated(token);
            }
        }
        Ok(())
    }
}

pub fn identify(controller: Uuid) -> Result<()> {
    unsafe {
        windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(&identity(controller))?;
    }
    Ok(())
}

fn identity(controller: Uuid) -> HSTRING {
    HSTRING::from(format!("Corvid.AgentDeck.{}", controller.simple()))
}

impl Drop for Sink {
    fn drop(&mut self) {
        for (toast, token) in &self.active {
            let _ = toast.RemoveActivated(*token);
        }
        if let Ok(history) = ToastNotificationManager::History() {
            let _ = history.ClearWithId(&self.id);
        }
    }
}

fn shortcut_path(folder: &GUID, controller: Uuid) -> Result<PathBuf> {
    let raw = unsafe { SHGetKnownFolderPath(folder, KF_FLAG_DEFAULT, None)? };
    let path = unsafe { raw.to_string() };
    unsafe {
        CoTaskMemFree(Some(raw.0.cast()));
    }
    Ok(PathBuf::from(path?).join(format!(
        "Corvid Agent Deck {}.lnk",
        &controller.simple().to_string()[..8]
    )))
}

fn save_shortcut(path: &Path, arguments: &str, id: &HSTRING) -> Result<()> {
    let executable = std::env::current_exe()?;
    let launcher = setup_launcher(&executable);
    let target = launcher.as_deref().unwrap_or(&executable);
    let target = wide(
        target
            .to_str()
            .context("Agent Deck shortcut target is not Unicode")?,
    );
    let icon = wide(
        executable
            .to_str()
            .context("Agent Deck executable path is not Unicode")?,
    );
    let arguments = wide(arguments);
    let path = wide(path.to_str().context("shortcut path is not Unicode")?);
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)? };
    unsafe {
        link.SetPath(PCWSTR(target.as_ptr()))?;
        link.SetArguments(PCWSTR(arguments.as_ptr()))?;
        link.SetDescription(&HSTRING::from(
            "Corvid Agent Deck background agent monitoring",
        ))?;
        link.SetIconLocation(PCWSTR(icon.as_ptr()), 0)?;
    }
    // PROPVARIANT's Drop calls PropVariantClear. Give it a COM allocation,
    // never a pointer into a Rust Vec.
    let id = unsafe { SHStrDupW(id)? };
    let value = PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_LPWSTR,
                Anonymous: PROPVARIANT_0_0_0 { pwszVal: id },
                ..Default::default()
            }),
        },
    };
    let property = PROPERTYKEY {
        fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
        pid: 5,
    };
    let store: IPropertyStore = link.cast()?;
    let file: IPersistFile = link.cast()?;
    unsafe {
        store.SetValue(&property, &value)?;
        store.Commit()?;
        file.Save(PCWSTR(path.as_ptr()), true)?;
    }
    Ok(())
}

fn setup_launcher(executable: &Path) -> Option<PathBuf> {
    let plugin = executable.parent()?;
    let plugins = plugin.parent()?;
    let version = plugins.parent()?;
    let versions = version.parent()?;
    let install = versions.parent()?;
    if plugin.file_name()? != "agent-deck"
        || plugins.file_name()? != "plugins"
        || versions.file_name()? != "versions"
        || std::fs::read_to_string(version.join("native-package"))
            .ok()?
            .trim()
            != "windows-setup"
    {
        return None;
    }
    let launcher = install.join("bin/corvid-agent-deck.exe");
    launcher.is_file().then_some(launcher)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn quote(value: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for character in value.chars() {
        if character == '\\' {
            slashes += 1;
            continue;
        }
        result.extend(std::iter::repeat_n(
            '\\',
            if character == '"' {
                slashes * 2 + 1
            } else {
                slashes
            },
        ));
        result.push(character);
        slashes = 0;
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_drops_an_owned_shortcut_app_identity() -> Result<()> {
        let _apartment = Apartment::new()?;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("Agent Deck fixture.lnk");
        let id = identity(Uuid::new_v4());
        save_shortcut(&path, "--background", &id)?;
        assert!(std::fs::metadata(&path)?.len() > 0);
        save_shortcut(&path, "--background", &id)?;
        Ok(())
    }

    #[test]
    #[ignore = "creates a temporary native notification and Start Menu registration"]
    fn native_notification_reaches_windows_history() -> Result<()> {
        let _apartment = Apartment::new()?;
        let directory = tempfile::tempdir()?;
        let controller = Uuid::new_v4();
        identify(controller)?;
        let (events, _received) = mpsc::channel(4);
        let shortcut = shortcut_path(&FOLDERID_Programs, controller)?;
        let outcome = (|| -> Result<()> {
            let mut sink = Sink::new(
                controller,
                directory.path(),
                "agent-deck-test",
                events,
                UiSignal::default(),
            )?;
            let notice = Notice {
                summary: true,
                key: crate::notifications::Key {
                    host: Uuid::new_v4(),
                    epoch: Uuid::new_v4(),
                    run: Uuid::new_v4(),
                    attention: Uuid::new_v4(),
                },
                title: "Corvid Agent Deck delivery check".into(),
                body: "Temporary test notification; automatically removed after verification."
                    .into(),
                created_ms: crate::notifications::now_ms(),
            };
            sink.show(notice, false)?;
            let history = ToastNotificationManager::History()
                .context("opening Windows notification history")?;
            for _ in 0..30 {
                if history
                    .GetHistoryWithId(&identity(controller))
                    .context("reading test notification history")?
                    .Size()?
                    > 0
                {
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            anyhow::bail!("Windows did not retain the test notification in its history")
        })();
        if shortcut.exists() {
            std::fs::remove_file(shortcut)?;
        }
        outcome
    }

    #[test]
    fn escapes_xml_and_quotes_windows_paths() {
        assert_eq!(escape("<&\"'"), "&lt;&amp;&quot;&apos;");
        assert_eq!(quote("C:\\Agent Deck\\"), "\"C:\\Agent Deck\\\\\"");
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
    }
}
