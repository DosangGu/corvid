use corvid_desktop::ipc::{Command, DesktopClient, Display, Link};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AgentBadge, AgentProvider, CompanionConnection, FocusSlot, PresentedBinding, PresentedSlot,
    RunAvailability,
};
use uuid::Uuid;

use crate::{KEY_COUNT, Tone};

const SLOTS_PER_PAGE: usize = KEY_COUNT - 1;

#[derive(Default)]
pub struct LiveDeck {
    display: Display,
    target: Option<PresentedBinding>,
    page: usize,
    revision: u64,
    pending: Option<(Uuid, String)>,
    pub result: Option<String>,
}

#[derive(Clone, PartialEq)]
pub struct Capture {
    pub index: usize,
    revision: u64,
    focus: Option<FocusSlot>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Face {
    pub label: String,
    pub detail: String,
    pub glyph: &'static str,
    pub tone: Tone,
    pub attention: u32,
}

impl LiveDeck {
    pub fn update(&mut self, display: Display) -> bool {
        let changed =
            self.display.link != display.link || self.display.snapshot != display.snapshot;
        let before = self.scope();
        self.display = display;
        if before != self.scope()
            || self
                .target
                .as_ref()
                .is_some_and(|target| !self.has_target(target))
        {
            self.target = None;
            self.revision += 1;
        }
        self.select_page(self.page.min(self.page_count() - 1));

        if let Some(completion) = &self.display.completion
            && let Some((id, label)) = &self.pending
            && *id == completion.id
        {
            self.result = Some(format!("{label} · {}", completion.message));
            self.pending = None;
            return true;
        }
        if self.scope().is_none()
            && let Some((_, label)) = self.pending.take()
        {
            self.result = Some(format!(
                "{label} · Focus outcome unknown. Check the terminal before trying again."
            ));
            return true;
        }
        changed
    }

    pub fn scope(&self) -> Option<(Uuid, Uuid)> {
        let snapshot = self.display.snapshot.as_ref()?;
        (self.display.link == Link::Online).then_some((snapshot.link, snapshot.generation()?))
    }

    pub fn status(&self) -> &'static str {
        match self.display.link {
            Link::Connecting => "Connecting to companion",
            Link::Unavailable => "Companion unavailable",
            Link::Incompatible => "Companion incompatible",
            Link::Online => {
                match self.display.snapshot.as_ref().and_then(|snapshot| {
                    CompanionConnection::try_from(snapshot.data.connection).ok()
                }) {
                    Some(CompanionConnection::Ready) => "Host ready",
                    Some(CompanionConnection::Syncing) => "Host syncing",
                    Some(CompanionConnection::Connecting) => "Connecting to host",
                    Some(CompanionConnection::Incompatible) => "Host incompatible",
                    Some(CompanionConnection::Error) => "Host error",
                    _ => "Host offline",
                }
            }
        }
    }

    #[cfg(windows)]
    pub fn can_pair(&self) -> bool {
        self.scope().is_some()
            && self
                .display
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.can_pair_wezterm)
            && self.pending.is_none()
    }

    #[cfg(windows)]
    pub fn terminal_hint(&self) -> &'static str {
        if self.display.link != Link::Online {
            return "Waiting for the local companion.";
        }
        let issue = self
            .display
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.data.issue_code.as_deref());
        match issue {
            Some("HostKeyRequired") => {
                "Verify the host key in OpenSSH, then leave this app open to reconnect."
            }
            Some("AuthenticationRequired") => {
                "Check that the configured SSH alias can connect without a password prompt."
            }
            Some("RelayUnavailable") => "Install corvid on the Linux host's command PATH.",
            Some("DaemonUnavailable") => "Open the working Corvid TUI to start its Linux daemon.",
            Some("HostMismatch") => "The host identity changed. Check the saved SSH alias.",
            Some("ProtocolMismatch") => "Update Corvid on the Linux host and Windows.",
            _ if self.scope().is_none() => "Waiting for the Linux host to synchronize.",
            _ => "Pair the working Corvid TUI to enable focus.",
        }
    }

    pub fn bindings(&self) -> &[PresentedBinding] {
        self.display
            .snapshot
            .as_ref()
            .filter(|_| self.scope().is_some())
            .map_or(&[], |snapshot| &snapshot.data.bindings)
    }

    pub fn target(&self) -> Option<&PresentedBinding> {
        self.target.as_ref()
    }

    fn has_target(&self, binding: &PresentedBinding) -> bool {
        self.scope().is_some()
            && self
                .display
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.contains_binding(binding))
    }

    pub fn select_target(&mut self, scope: (Uuid, Uuid), binding: PresentedBinding) {
        if self.scope() == Some(scope)
            && self.has_target(&binding)
            && self.target.as_ref() != Some(&binding)
        {
            self.target = Some(binding);
            self.revision += 1;
        }
    }

    pub fn page(&self) -> usize {
        self.page
    }

    pub fn page_count(&self) -> usize {
        self.display
            .snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.data.slots.len())
            .div_ceil(SLOTS_PER_PAGE)
            .max(1)
    }

    pub fn overflow(&self) -> u32 {
        self.display
            .snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.data.overflow_count)
    }

    pub fn select_page(&mut self, page: usize) {
        if page < self.page_count() && page != self.page {
            self.page = page;
            self.revision += 1;
        }
    }

    pub fn shift_page(&mut self, forward: bool) {
        let pages = self.page_count();
        let offset = if forward { 1 } else { pages - 1 };
        self.select_page((self.page + offset) % pages);
    }

    fn slot(&self, index: usize) -> Option<&PresentedSlot> {
        if index >= SLOTS_PER_PAGE {
            return None;
        }
        self.display
            .snapshot
            .as_ref()?
            .data
            .slots
            .get(self.page * SLOTS_PER_PAGE + index)
    }

    pub fn face(&self, index: usize) -> Face {
        if index == SLOTS_PER_PAGE {
            return Face {
                label: if self.page_count() > 1 {
                    "Next page"
                } else {
                    "Agents"
                }
                .into(),
                detail: format!("{} / {}", self.page + 1, self.page_count()),
                glyph: "page",
                tone: Tone::Blue,
                attention: 0,
            };
        }
        let slot = self.slot(index);
        let Some(run) = slot.and_then(|slot| slot.run.as_ref()) else {
            return Face {
                label: format!("Slot {:02}", self.page * SLOTS_PER_PAGE + index + 1),
                detail: match slot {
                    Some(slot) if slot.pinned => "Pinned · Empty",
                    Some(_) => "Empty",
                    None if self.display.snapshot.is_none() => "Awaiting slots",
                    None => "Unused",
                }
                .into(),
                glyph: "grid",
                tone: Tone::Slate,
                attention: 0,
            };
        };
        let (name, glyph) = match AgentProvider::try_from(run.provider) {
            Ok(AgentProvider::Claude) => ("Claude", "brand"),
            Ok(AgentProvider::Codex) => ("Codex", "focus"),
            _ => ("Agent", "activity"),
        };
        let (detail, tone) = match RunAvailability::try_from(run.availability) {
            Ok(RunAvailability::Historical) => ("Historical".into(), Tone::Slate),
            Ok(RunAvailability::Offline) => ("Cached".into(), Tone::Slate),
            Ok(RunAvailability::Available) if self.scope().is_some() => {
                match AgentBadge::try_from(run.last_badge) {
                    Ok(AgentBadge::NeedsInput) => (
                        format!("{} waiting", run.pending_requests.max(1)),
                        Tone::Amber,
                    ),
                    Ok(AgentBadge::Working) => ("Working".into(), Tone::Blue),
                    Ok(AgentBadge::ResponseObserved) => ("Responded".into(), Tone::Green),
                    Ok(AgentBadge::Ready) => ("Ready".into(), Tone::Violet),
                    Ok(AgentBadge::Interrupted) => ("Interrupted".into(), Tone::Rose),
                    Ok(AgentBadge::Error) => ("Error".into(), Tone::Rose),
                    Ok(AgentBadge::Ended) => ("Ended".into(), Tone::Slate),
                    _ => ("Check needed".into(), Tone::Slate),
                }
            }
            Ok(RunAvailability::Available) => ("Cached".into(), Tone::Slate),
            _ => ("Check needed".into(), Tone::Slate),
        };
        Face {
            label: format!("{name} {:02}", slot.expect("run belongs to slot").slot_id),
            detail,
            glyph,
            tone,
            attention: run.unseen_attention,
        }
    }

    pub fn capture(&self, index: usize) -> Option<Capture> {
        let focus = if index == SLOTS_PER_PAGE && self.page_count() > 1 {
            None
        } else {
            self.scope()?;
            if self.pending.is_some() {
                return None;
            }
            let snapshot = self.display.snapshot.as_ref()?;
            let Body::FocusSlot(focus) =
                snapshot.focus(self.slot(index)?, self.target.as_ref()?)?
            else {
                return None;
            };
            Some(focus)
        };
        Some(Capture {
            index,
            revision: self.revision,
            focus,
        })
    }

    pub fn release(&mut self, client: &DesktopClient, capture: Capture) {
        if !self.accepts(&capture) {
            return;
        }
        let Some(focus) = capture.focus else {
            self.shift_page(true);
            return;
        };
        let Some((link, generation)) = self.scope() else {
            return;
        };
        let label = self.face(capture.index).label;
        let id = Uuid::new_v4();
        let command = Command {
            id,
            link,
            generation,
            page: 0,
            body: Body::FocusSlot(focus),
        };
        if client.commands.try_send(command).is_ok() {
            self.result = Some(format!("Selecting {label} in the registered terminal…"));
            self.pending = Some((id, label));
        } else {
            self.result = Some("Companion busy. Try again after it refreshes.".into());
        }
    }

    fn accepts(&self, capture: &Capture) -> bool {
        self.capture(capture.index).as_ref() == Some(capture)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use corvid_desktop::model::Snapshot;
    use corvid_protocol::wire::{PresentationSlots, PresentedRun, PresentedRunKey};

    use super::*;

    fn setup() -> (LiveDeck, Display) {
        let host = Uuid::new_v4();
        let remote_epoch = Uuid::new_v4().to_string();
        let mut slots: Vec<_> = (1..=15)
            .map(|slot_id| PresentedSlot {
                slot_id,
                revision: 1,
                ..Default::default()
            })
            .collect();
        slots[0].run = Some(PresentedRun {
            key: Some(PresentedRunKey {
                host_id: host.to_string(),
                remote_epoch: remote_epoch.clone(),
                agent_run_id: Uuid::new_v4().to_string(),
            }),
            availability: RunAvailability::Available as i32,
            provider: AgentProvider::Claude as i32,
            last_badge: AgentBadge::Working as i32,
            ..Default::default()
        });
        let display = Display {
            link: Link::Online,
            snapshot: Some(Arc::new(Snapshot {
                host,
                link: Uuid::new_v4(),
                page: 0,
                data: PresentationSlots {
                    connection: CompanionConnection::Ready as i32,
                    control_generation: Some(Uuid::new_v4().to_string()),
                    remote_epoch: Some(remote_epoch),
                    slots,
                    bindings: vec![PresentedBinding {
                        client_id: Uuid::new_v4().to_string(),
                        revision: (1 << 53) + 1,
                        wezterm: None,
                    }],
                    ..Default::default()
                },
                #[cfg(windows)]
                can_pair_wezterm: true,
            })),
            ..Default::default()
        };
        let mut deck = LiveDeck::default();
        deck.update(display.clone());
        assert!(deck.capture(0).is_none());
        deck.select_target(deck.scope().unwrap(), deck.bindings()[0].clone());
        (deck, display)
    }

    fn update_snapshot(display: &Display, edit: impl FnOnce(&mut Snapshot)) -> Display {
        let mut updated = display.clone();
        let snapshot = display.snapshot.as_ref().unwrap();
        let mut snapshot = Snapshot {
            data: snapshot.data.clone(),
            host: snapshot.host,
            link: snapshot.link,
            page: snapshot.page,
            #[cfg(windows)]
            can_pair_wezterm: snapshot.can_pair_wezterm,
        };
        edit(&mut snapshot);
        updated.snapshot = Some(Arc::new(snapshot));
        updated
    }

    #[test]
    fn status_update_preserves_a_capture_but_reassignment_cancels_it() {
        let (mut deck, display) = setup();
        let press = deck.capture(0).unwrap();
        let updated = update_snapshot(&display, |snapshot| {
            let run = snapshot.data.slots[0].run.as_mut().unwrap();
            run.last_badge = AgentBadge::NeedsInput as i32;
            run.pending_requests = 2;
            run.unseen_attention = 1;
        });
        deck.update(updated.clone());
        assert!(deck.accepts(&press));
        assert_eq!(deck.face(0).detail, "2 waiting");
        assert_eq!(deck.face(0).attention, 1);
        assert_eq!(
            press
                .focus
                .as_ref()
                .unwrap()
                .binding
                .as_ref()
                .unwrap()
                .revision,
            (1 << 53) + 1
        );

        deck.update(update_snapshot(&updated, |snapshot| {
            let slot = &mut snapshot.data.slots[0];
            slot.revision += 1;
            slot.run
                .as_mut()
                .unwrap()
                .key
                .as_mut()
                .unwrap()
                .agent_run_id = Uuid::new_v4().to_string();
        }));
        assert!(!deck.accepts(&press));
        assert_eq!(deck.page(), 0);
    }

    #[test]
    fn cancels_on_page_change_repair_and_reconnect() {
        let (mut deck, display) = setup();
        let press = deck.capture(0).unwrap();
        deck.shift_page(true);
        deck.shift_page(false);
        assert!(!deck.accepts(&press));

        let press = deck.capture(0).unwrap();
        deck.update(update_snapshot(&display, |snapshot| {
            snapshot.data.bindings[0].revision += 1
        }));
        assert!(!deck.accepts(&press));
        assert!(deck.target().is_none());
        deck.select_target(deck.scope().unwrap(), deck.bindings()[0].clone());
        let press = deck.capture(0).unwrap();
        deck.update(Display {
            link: Link::Unavailable,
            ..display.clone()
        });
        assert_eq!(deck.face(0).detail, "Cached");
        assert!(!deck.accepts(&press));
        deck.update(display);
        assert!(deck.target().is_none());
        assert!(deck.capture(0).is_none());
    }
}
