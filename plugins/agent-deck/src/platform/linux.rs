use std::path::Path;
use std::time::Duration;

use async_process::Command;
use corvid_agent_deck_core::native::{
    Availability, Capabilities, NativeError, NativeServices, Status,
};
use corvid_agent_deck_core::notifications::Notice;
use corvid_async::time::timeout;
use uuid::Uuid;

use super::Callbacks;

pub(crate) struct Linux {
    _capabilities: corvid_async::watch::Sender<Capabilities>,
}

pub(crate) fn start(callbacks: Callbacks, _: Uuid, _: &Path, _: &Path) -> anyhow::Result<Linux> {
    callbacks.capabilities.send_replace(Capabilities {
        notifications: Availability::Ready,
        ..Default::default()
    });
    Ok(Linux {
        _capabilities: callbacks.capabilities,
    })
}

impl NativeServices for Linux {
    fn update_status(&mut self, _: Status) -> Result<(), NativeError> {
        Err(NativeError::Unsupported)
    }

    async fn set_login_startup(&mut self, _: bool) -> Result<(), NativeError> {
        Err(NativeError::Unsupported)
    }

    async fn notify(&mut self, notice: Notice, sound: bool) -> Result<(), NativeError> {
        if sound {
            return Err(NativeError::Unsupported);
        }
        let mut command = Command::new("notify-send");
        command
            .args([
                "--app-name=Corvid Agent Deck",
                "--icon=corvid",
                "--",
                &notice.title,
                &notice.body,
            ])
            .kill_on_drop(true);
        let status = timeout(Duration::from_secs(3), command.status())
            .await
            .map_err(|_| NativeError::TimedOut)?
            .map_err(|error| {
                NativeError::Rejected(format!("Could not run notify-send: {error}"))
            })?;
        if !status.success() {
            return Err(NativeError::Rejected(
                "Linux notification service is unavailable".into(),
            ));
        }
        Ok(())
    }
}
