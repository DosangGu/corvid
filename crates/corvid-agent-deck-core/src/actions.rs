use std::sync::Arc;
use std::time::{Duration, Instant};

use corvid_presentation::ipc::{Command, Completion, DesktopClient};
use corvid_presentation::model::Snapshot;
use corvid_protocol::wire::{PresentedRunKey, envelope::Body};
use uuid::Uuid;

#[derive(Clone)]
pub enum FocusAction {
    Desktop(PresentedRunKey),
    HostDesktop {
        controller: Uuid,
        run: PresentedRunKey,
    },
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

    pub fn submit(&mut self, client: &DesktopClient, page: u32, snapshot: &Snapshot, body: Body) {
        if self.is_pending() {
            return;
        }
        let Some(generation) = snapshot.generation() else {
            return;
        };
        let id = Uuid::new_v4();
        let command = Command {
            id,
            link: snapshot.link,
            page,
            generation,
            body,
        };
        if !client.display.borrow().accepts(&command) {
            self.notice = Some(Completion {
                id,
                message: "The display changed during the gesture. Review the slots and try again."
                    .to_owned(),
                failed: true,
                paired: None,
            });
            return;
        }
        if client.commands.try_send(command).is_ok() {
            self.pending = Some(id);
            self.pending_since = Some(Instant::now());
            self.notice = None;
        } else {
            self.notice = Some(Completion {
                id,
                message: "Could not send the action. Wait for the companion and try again."
                    .to_owned(),
                failed: true,
                paired: None,
            });
        }
    }

    pub fn focus(
        &mut self,
        key: PresentedRunKey,
        send: impl FnOnce(Uuid, PresentedRunKey) -> bool,
    ) {
        if self.is_pending() {
            return;
        }
        let id = Uuid::new_v4();
        if send(id, key) {
            self.pending = Some(id);
            self.pending_since = Some(Instant::now());
            self.notice = None;
        } else {
            self.notice = Some(Completion {
                id,
                message: "Could not send the action to Desktop.".into(),
                failed: true,
                paired: None,
            });
        }
    }
}
