use anyhow::{Result, ensure};
use corvid_companion::{MAX_SLOTS, PRESENTATION_PAGE_SIZE};
use corvid_protocol::Session;
use corvid_protocol::wire::edit_slot_layout::Action;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AckPresentedAttention, CompanionConnection, EditSlotLayout, FocusSlot, PinSlot, PlaceSlot,
    PresentationSlots, PresentedBinding, PresentedRun, PresentedRunKey, PresentedSlot,
    PresentedSlotToken, RunAvailability,
};
use uuid::Uuid;

pub(crate) struct Snapshot {
    pub(crate) link: Uuid,
    pub(crate) host: Uuid,
    pub(crate) page: u32,
    pub(crate) data: PresentationSlots,
}

impl Snapshot {
    pub(crate) fn new(
        link: Uuid,
        session: &Session,
        page: u32,
        data: PresentationSlots,
    ) -> Result<Self> {
        let host = Uuid::parse_str(&session.host_id)?;
        let page_size = PRESENTATION_PAGE_SIZE as usize;
        ensure!(
            (1..=MAX_SLOTS as usize).contains(&data.slots.len())
                && data.overflow.len() <= page_size
                && data.suppressed.len() <= page_size
                && data.recent_ended.len() <= page_size,
            "invalid presentation collection size"
        );
        for (index, slot) in data.slots.iter().enumerate() {
            ensure!(
                slot.slot_id == index as u32 + 1 && slot.revision != 0,
                "invalid presentation slot identity"
            );
        }
        Ok(Self {
            link,
            host,
            page,
            data,
        })
    }

    pub(crate) fn generation(&self) -> Option<Uuid> {
        if self.data.connection != CompanionConnection::Ready as i32 {
            return None;
        }
        Uuid::parse_str(self.data.control_generation.as_deref()?).ok()
    }

    pub(crate) fn page_count(&self) -> u32 {
        self.data
            .overflow_count
            .max(self.data.suppressed_count)
            .div_ceil(PRESENTATION_PAGE_SIZE)
            .max(1)
    }

    pub(crate) fn focus(&self, slot: &PresentedSlot, binding: &PresentedBinding) -> Option<Body> {
        self.live_key(slot.run.as_ref()?)?;
        self.contains_binding(binding)
            .then_some(Body::FocusSlot(FocusSlot {
                control_generation: self.generation()?.to_string(),
                slot: Some(slot_token(slot)),
                binding: Some(PresentedBinding {
                    client_id: binding.client_id.clone(),
                    revision: binding.revision,
                    wezterm: None,
                }),
            }))
    }

    pub(crate) fn contains_binding(&self, binding: &PresentedBinding) -> bool {
        binding.revision != 0
            && Uuid::parse_str(&binding.client_id).is_ok()
            && self.data.bindings.iter().any(|entry| {
                entry.client_id == binding.client_id && entry.revision == binding.revision
            })
    }

    pub(crate) fn acknowledge(&self, run: &PresentedRun) -> Option<Body> {
        let key = self.live_key(run)?;
        let attention = Uuid::parse_str(run.latest_unseen_attention_id.as_deref()?).ok()?;
        Some(Body::AckPresentedAttention(AckPresentedAttention {
            control_generation: self.generation()?.to_string(),
            run: Some(key.clone()),
            attention_id: attention.to_string(),
        }))
    }

    pub(crate) fn pin(&self, slot: &PresentedSlot) -> Option<Body> {
        self.edit(Action::Pin(PinSlot {
            slot: Some(slot_token(slot)),
            pinned: Some(!slot.pinned),
        }))
    }

    pub(crate) fn remove(&self, slot: &PresentedSlot) -> Option<Body> {
        self.check_key(slot.run.as_ref()?.key.as_ref()?)?;
        self.edit(Action::Remove(slot_token(slot)))
    }

    pub(crate) fn place(&self, slot: &PresentedSlot, key: &PresentedRunKey) -> Option<Body> {
        if slot.run.is_some() || !self.can_place(key) {
            return None;
        }
        self.edit(Action::Place(PlaceSlot {
            slot: Some(slot_token(slot)),
            run: Some(key.clone()),
        }))
    }

    pub(crate) fn can_place(&self, key: &PresentedRunKey) -> bool {
        self.generation().is_some()
            && self
                .data
                .overflow
                .iter()
                .any(|run| self.live_key(run) == Some(key))
    }

    pub(crate) fn remove_overflow(&self, run: &PresentedRun) -> Option<Body> {
        let key = run.key.as_ref()?;
        self.check_key(key)?;
        self.edit(Action::RemoveOverflow(key.clone()))
    }

    pub(crate) fn readd(&self, key: &PresentedRunKey) -> Option<Body> {
        self.check_key(key)?;
        if self.data.remote_epoch.as_deref()? != key.remote_epoch {
            return None;
        }
        self.edit(Action::Readd(key.clone()))
    }

    fn edit(&self, action: Action) -> Option<Body> {
        self.generation()?;
        Some(Body::EditSlotLayout(EditSlotLayout {
            action: Some(action),
        }))
    }

    fn live_key<'a>(&self, run: &'a PresentedRun) -> Option<&'a PresentedRunKey> {
        let key = run.key.as_ref()?;
        self.check_key(key)?;
        if run.availability != RunAvailability::Available as i32
            || self.data.remote_epoch.as_deref()? != key.remote_epoch
        {
            return None;
        }
        Some(key)
    }

    fn check_key(&self, key: &PresentedRunKey) -> Option<()> {
        if Uuid::parse_str(&key.host_id).ok()? != self.host {
            return None;
        }
        Uuid::parse_str(&key.remote_epoch).ok()?;
        Uuid::parse_str(&key.agent_run_id).ok()?;
        Some(())
    }
}

fn slot_token(slot: &PresentedSlot) -> PresentedSlotToken {
    PresentedSlotToken {
        slot_id: slot.slot_id,
        revision: Some(slot.revision),
        run: slot.run.as_ref().and_then(|run| run.key.clone()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use corvid_protocol::{PROTOCOL_MINOR, PeerRole};

    use super::*;

    pub(crate) fn ready_snapshot() -> Snapshot {
        let host = Uuid::new_v4().to_string();
        let epoch = Uuid::new_v4().to_string();
        let session = Session {
            role: PeerRole::Presentation,
            protocol_minor: PROTOCOL_MINOR,
            host_id: host.clone(),
            server_epoch: Uuid::new_v4().to_string(),
            controller_id: None,
            capabilities: Vec::new(),
            provider_profiles: Vec::new(),
        };
        let data = PresentationSlots {
            connection: CompanionConnection::Ready as i32,
            remote_epoch: Some(epoch.clone()),
            control_generation: Some(Uuid::new_v4().to_string()),
            slots: vec![PresentedSlot {
                slot_id: 1,
                revision: (1 << 53) + 1,
                run: Some(PresentedRun {
                    availability: RunAvailability::Available as i32,
                    key: Some(PresentedRunKey {
                        host_id: host,
                        remote_epoch: epoch,
                        agent_run_id: Uuid::new_v4().to_string(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            bindings: vec![PresentedBinding {
                client_id: Uuid::new_v4().to_string(),
                revision: (1 << 53) + 3,
                wezterm: None,
            }],
            ..Default::default()
        };
        Snapshot::new(Uuid::new_v4(), &session, 0, data).unwrap()
    }

    #[test]
    fn preserves_the_displayed_run_and_u64_revisions() {
        let snapshot = ready_snapshot();
        let slot = &snapshot.data.slots[0];
        let binding = &snapshot.data.bindings[0];
        let Some(Body::FocusSlot(request)) = snapshot.focus(slot, binding) else {
            panic!("missing focus request");
        };
        let captured = request.slot.unwrap();
        assert_eq!(captured.run, slot.run.as_ref().unwrap().key);
        assert_eq!(captured.revision, Some(9_007_199_254_740_993));
        assert_eq!(request.binding.unwrap().revision, 9_007_199_254_740_995);
    }

    #[test]
    fn disables_focus_for_unrecognized_availability_or_a_replaced_binding() {
        let mut snapshot = ready_snapshot();
        let mut stale = snapshot.data.bindings[0].clone();
        stale.revision -= 1;
        assert!(snapshot.focus(&snapshot.data.slots[0], &stale).is_none());

        snapshot.data.slots[0].run.as_mut().unwrap().availability = i32::MAX;
        let binding = &snapshot.data.bindings[0];
        assert!(snapshot.focus(&snapshot.data.slots[0], binding).is_none());
    }
}
