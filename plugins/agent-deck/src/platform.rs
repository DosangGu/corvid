use corvid_agent_deck_core::native::{Capabilities, Event, NativeError};
use corvid_async::{mpsc, watch};

use crate::ui_signal::UiSignal;

// Linux has no native callbacks.
#[cfg_attr(target_os = "linux", allow(dead_code))]
#[derive(Clone)]
pub(crate) struct Callbacks {
    pub events: mpsc::Sender<Event>,
    pub issues: watch::Sender<Option<NativeError>>,
    pub capabilities: watch::Sender<Capabilities>,
    pub signal: UiSignal,
}

#[cfg(any(not(target_os = "linux"), test))]
impl Callbacks {
    pub fn event(&self, event: Event) -> Result<(), NativeError> {
        use mpsc::error::TrySendError;
        self.events.try_send(event).map_err(|error| match error {
            TrySendError::Full(_) => {
                self.issue(NativeError::QueueFull);
                NativeError::QueueFull
            }
            TrySendError::Closed(_) => NativeError::Disconnected,
        })
    }

    pub fn issue(&self, error: NativeError) {
        self.issues.send_replace(Some(error));
    }
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::start;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::{shutdown_on_main, start, yield_activation, yield_card_activation};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{identify, start};
