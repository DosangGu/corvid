use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Presentation {
    Open,
    #[cfg(target_os = "macos")]
    Toggle,
    #[cfg(target_os = "macos")]
    Hide,
}

/// Coalesced worker notifications and latched lifecycle requests. The receiver
/// runs on the application executor even when the native surface is hidden.
#[derive(Clone)]
pub(crate) struct UiSignal {
    changed: watch::Sender<()>,
    presentation: Arc<Mutex<Option<Presentation>>>,
    quit: Arc<AtomicBool>,
}

impl Default for UiSignal {
    fn default() -> Self {
        Self {
            changed: watch::channel(()).0,
            presentation: Arc::default(),
            quit: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl UiSignal {
    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.changed.subscribe()
    }

    pub fn notify(&self) {
        self.changed.send_replace(());
    }

    pub fn open(&self) {
        *self.presentation.lock().unwrap() = Some(Presentation::Open);
        self.notify();
    }

    pub fn take_presentation(&self) -> Option<Presentation> {
        self.presentation.lock().unwrap().take()
    }

    #[cfg(target_os = "macos")]
    pub fn toggle(&self) {
        let mut pending = self.presentation.lock().unwrap();
        *pending = match *pending {
            Some(Presentation::Open) => Some(Presentation::Hide),
            Some(Presentation::Hide) => Some(Presentation::Open),
            Some(Presentation::Toggle) => None,
            None => Some(Presentation::Toggle),
        };
        drop(pending);
        self.notify();
    }

    pub fn quit(&self) {
        self.quit.store(true, Ordering::Release);
        self.notify();
    }

    pub fn quitting(&self) -> bool {
        self.quit.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lifecycle_requests_wake_a_hidden_ui_and_quit_stays_latched() {
        let signal = UiSignal::default();
        let mut receiver = signal.subscribe();
        signal.quit();
        receiver.changed().await.unwrap();
        signal.open();
        assert_eq!(signal.take_presentation(), Some(Presentation::Open));
        assert_eq!(signal.take_presentation(), None);
        assert!(signal.quitting());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn status_clicks_toggle_without_losing_an_explicit_open() {
        let signal = UiSignal::default();
        signal.toggle();
        signal.toggle();
        assert_eq!(signal.take_presentation(), None);

        signal.open();
        signal.toggle();
        assert_eq!(signal.take_presentation(), Some(Presentation::Hide));

        signal.toggle();
        signal.open();
        assert_eq!(signal.take_presentation(), Some(Presentation::Open));
    }
}
