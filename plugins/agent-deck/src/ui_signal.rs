use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::watch;

/// Coalesced worker notifications and latched lifecycle requests. The receiver
/// runs on GPUI's foreground executor even when the native window is hidden.
#[derive(Clone)]
pub(crate) struct UiSignal {
    changed: watch::Sender<()>,
    open: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
}

impl Default for UiSignal {
    fn default() -> Self {
        Self {
            changed: watch::channel(()).0,
            open: Arc::new(AtomicBool::new(false)),
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
        self.open.store(true, Ordering::Release);
        self.notify();
    }

    pub fn take_open(&self) -> bool {
        self.open.swap(false, Ordering::AcqRel)
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
        assert!(signal.take_open());
        assert!(!signal.take_open());
        assert!(signal.quitting());
    }
}
