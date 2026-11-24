use std::sync::Arc;
use std::time::{Duration, Instant};

use corvid_presentation::ipc::Completion;
use corvid_presentation::model::Snapshot;
use corvid_protocol::wire::{PresentedRunKey, envelope::Body};
use uuid::Uuid;

#[derive(Clone)]
pub enum FocusAction {
    Desktop(Arc<Snapshot>, PresentedRunKey),
    Companion(Arc<Snapshot>, Box<Body>),
}

#[derive(Default)]
pub struct Actions {
    pending: Option<Uuid>,
    pending_since: Option<Instant>,
    notice: Option<Completion>,
}

impl Actions {
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn notice(&self) -> Option<&Completion> {
        self.notice.as_ref()
    }

    pub fn complete(&mut self, completion: Completion) {
        if self.pending == Some(completion.id) {
            self.pending = None;
            self.notice = Some(completion);
        }
    }

    pub fn expire(&mut self) {
        if self.pending.is_some()
            && self
                .pending_since
                .is_some_and(|time| time.elapsed() > Duration::from_secs(8))
        {
            let id = self.pending.take().unwrap();
            self.notice = Some(Completion {
                id,
                message:
                    "The action was not confirmed. Review the current state before trying again."
                        .into(),
                failed: true,
                paired: None,
            });
        }
    }

    pub fn send(&mut self, send: impl FnOnce(Uuid) -> bool) {
        if self.is_pending() {
            return;
        }
        let id = Uuid::new_v4();
        if send(id) {
            self.pending = Some(id);
            self.pending_since = Some(Instant::now());
            self.notice = None;
        } else {
            self.notice = Some(Completion {
                id,
                message: "Could not send the action. Wait for the companion and try again.".into(),
                failed: true,
                paired: None,
            });
        }
    }
}
