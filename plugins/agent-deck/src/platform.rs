use crate::notifications::Notice;

#[derive(Clone)]
// Linux currently delivers notifications without tray or activation callbacks.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub enum Event {
    Open,
    Notifications,
    Pause,
    Quit,
    Notice(Notice),
    Error(String),
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Native, identify};

#[cfg(target_os = "linux")]
pub struct Native;

#[cfg(target_os = "linux")]
impl Native {
    pub fn start(
        _: tokio::sync::mpsc::Sender<Event>,
        _: eframe::egui::Context,
        _: uuid::Uuid,
        _: &std::path::Path,
        _: &std::path::Path,
    ) -> anyhow::Result<Self> {
        Ok(Self)
    }
    pub fn background(&self) -> bool {
        false
    }
    pub fn status(&self, _: &str, _: bool) {}
    pub fn startup(&self, enabled: bool) -> anyhow::Result<()> {
        anyhow::ensure!(
            !enabled,
            "Agent Deck login startup is currently available on Windows"
        );
        Ok(())
    }
    pub async fn notify(&self, notice: Notice, _: bool) -> anyhow::Result<()> {
        let mut command = tokio::process::Command::new("notify-send");
        command
            .args([
                "--app-name=Corvid Agent Deck",
                "--icon=dialog-information",
                "--",
                &notice.title,
                &notice.body,
            ])
            .kill_on_drop(true);
        let status =
            tokio::time::timeout(std::time::Duration::from_secs(3), command.status()).await??;
        anyhow::ensure!(
            status.success(),
            "Linux notification service is unavailable"
        );
        Ok(())
    }
}
