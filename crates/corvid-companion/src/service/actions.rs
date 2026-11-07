use std::collections::BTreeSet;

use corvid_client::{ControllerRequest, ControllerScope};
use corvid_protocol::ProtocolFault;
use corvid_protocol::wire::edit_slot_layout::Action;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AckPresentedAttention, FocusSlot, LocalActivation, PairTerminal, PresentedBinding,
    PresentedSlotToken, SlotFocused, SlotLayoutEdited,
};
use uuid::Uuid;

#[cfg(windows)]
use super::TerminalRecord;
use super::{Outcome, Remote, Service, observations, store_fault};
#[cfg(windows)]
use crate::platform::WezTermPane;
use crate::remote::{invalid, parse_id};
use crate::{RunKey, SlotLayout};

impl Service {
    pub(super) async fn execute(&mut self, body: Body) -> Outcome {
        match body {
            Body::GetPresentationSlots(query) => self.presentation(query),
            Body::PairTerminal(pair) => self.pair(pair).await,
            Body::UnpairTerminal(unpair) => {
                let remote = self.require_generation(&unpair.control_generation)?;
                let (client, revision) = require_binding(remote, unpair.binding.as_ref())?;
                let reply = self
                    .command(remote, ControllerRequest::Unregister { client, revision })
                    .await?;
                match reply {
                    Body::ClientBindingUnregistered(_) => {
                        #[cfg(windows)]
                        if self.terminals.remove(&client).is_some() {
                            self.save_terminals();
                        }
                        Ok(reply)
                    }
                    _ => Err(invalid_reply()),
                }
            }
            Body::FocusSlot(focus) => self.focus(focus).await,
            Body::AckPresentedAttention(ack) => self.acknowledge(ack).await,
            Body::EditSlotLayout(edit) => self.edit(edit.action.ok_or_else(invalid)?),
            _ => Err(invalid()),
        }
    }

    async fn pair(&mut self, pair: PairTerminal) -> Outcome {
        let remote = self.require_ready()?;
        let code = parse_id(&pair.code)?;
        #[cfg(not(windows))]
        if pair.wezterm.is_some() {
            return Err(ProtocolFault::new(
                "UnsupportedCapability",
                "WezTerm registration requires the Windows companion",
            ));
        }
        #[cfg(windows)]
        let pane = match pair.wezterm {
            Some(target) => Some(
                WezTermPane::capture(target)
                    .await
                    .map_err(|_| local_unavailable())?,
            ),
            None => None,
        };
        let reply = self
            .command(remote, ControllerRequest::Register { code })
            .await?;
        let Body::ClientBindingRegistered(binding) = &reply else {
            return Err(invalid_reply());
        };
        let client = parse_id(&binding.client_id).map_err(|_| invalid_reply())?;
        if binding.binding_revision == 0 || binding.commit_sequence < remote.sequence {
            return Err(invalid_reply());
        }
        #[cfg(windows)]
        if let Some(pane) = pane {
            self.terminals.insert(
                client,
                TerminalRecord {
                    epoch: remote.epoch,
                    revision: binding.binding_revision,
                    commit_sequence: binding.commit_sequence,
                    pane,
                },
            );
            self.save_terminals();
        } else if self.terminals.remove(&client).is_some() {
            self.save_terminals();
        }
        #[cfg(not(windows))]
        let _ = client;
        Ok(reply)
    }

    async fn focus(&self, focus: FocusSlot) -> Outcome {
        let remote = self.require_generation(&focus.control_generation)?;
        let token = SlotToken::parse(focus.slot.as_ref())?;
        let key = token.run.ok_or_else(invalid)?;
        token.check(self.store.layout())?;
        if !remote
            .runs
            .get(&key)
            .is_some_and(|run| run.observation.navigable)
        {
            return Err(ProtocolFault::new(
                "TargetGone",
                "displayed run is unavailable",
            ));
        }
        let (client, revision) = require_binding(remote, focus.binding.as_ref())?;
        #[cfg(windows)]
        let pane = match self.local_pane(remote, client, revision) {
            Some(saved) => Some(
                WezTermPane::restore(saved)
                    .await
                    .map_err(|_| local_unavailable())?,
            ),
            None => None,
        };
        let command = ControllerRequest::Focus {
            client,
            run: key.run,
            revision,
        };
        match self.command(remote, command).await? {
            Body::FocusApplied(applied)
                if parse_id(&applied.agent_run_id)? == key.run
                    && parse_id(&applied.client_id)? == client
                    && parse_id(&applied.terminal_id).is_ok()
                    && applied.selection.is_some()
                    && applied.view_revision != 0
                    && applied.commit_sequence >= remote.sequence =>
            {
                #[cfg(windows)]
                let (activation, wezterm) = match pane {
                    Some(pane) => (pane.activate().await, Some(pane.location())),
                    None => (LocalActivation::ManualRequired, None),
                };
                #[cfg(not(windows))]
                let (activation, wezterm) = (LocalActivation::ManualRequired, None);
                Ok(Body::SlotFocused(SlotFocused {
                    remote: Some(applied),
                    local_activation: activation as i32,
                    wezterm,
                }))
            }
            _ => Err(invalid_reply()),
        }
    }

    async fn acknowledge(&self, ack: AckPresentedAttention) -> Outcome {
        let remote = self.require_generation(&ack.control_generation)?;
        let key = RunKey::try_from(ack.run.as_ref().ok_or_else(invalid)?)?;
        if !remote.runs.contains_key(&key) {
            return Err(ProtocolFault::new(
                "TargetGone",
                "displayed run is unavailable",
            ));
        }
        let attention = parse_id(&ack.attention_id)?;
        let command = ControllerRequest::Acknowledge {
            run: key.run,
            attention,
        };
        let reply = self.command(remote, command).await?;
        match reply {
            Body::AttentionAcknowledged(ref ack)
                if parse_id(&ack.agent_run_id)? == key.run
                    && parse_id(&ack.attention_id)? == attention =>
            {
                Ok(reply)
            }
            _ => Err(invalid_reply()),
        }
    }

    async fn command(&self, remote: &Remote, command: ControllerRequest) -> Outcome {
        let scope = ControllerScope {
            controller_id: self.controller.clone(),
            host_id: self.host.clone(),
            server_epoch: remote.epoch.to_string(),
        };
        self.endpoint
            .controller(&scope, Uuid::new_v4().to_string(), command)
            .await
            .map_err(|error| match error.downcast::<ProtocolFault>() {
                Ok(fault) if fault.code != "Timeout" => fault,
                _ => ProtocolFault::new(
                    "OutcomeUnknown",
                    "remote command result unavailable; refresh the display",
                ),
            })
    }

    fn edit(&mut self, action: Action) -> Outcome {
        let runs = self.require_ready().ok().map(observations);
        self.store
            .update(|layout| {
                match action {
                    Action::Pin(pin) => {
                        let token = SlotToken::parse(pin.slot.as_ref())?;
                        layout.pin(
                            token.id,
                            token.revision,
                            token.run,
                            pin.pinned.ok_or_else(invalid)?,
                        )?;
                    }
                    Action::Remove(token) => {
                        let token = SlotToken::parse(Some(&token))?;
                        token.check(layout)?;
                        layout.remove(token.run.ok_or_else(invalid)?)?;
                    }
                    Action::RemoveOverflow(run) => {
                        let key = RunKey::try_from(&run)?;
                        if !layout.overflow().any(|run| run == key) {
                            return Err(stale().into());
                        }
                        layout.remove(key)?;
                    }
                    Action::Readd(run) => {
                        let key = RunKey::try_from(&run)?;
                        if !runs
                            .as_ref()
                            .ok_or_else(connection_required)?
                            .get(&key)
                            .is_some_and(|run| run.eligible)
                        {
                            return Err(stale().into());
                        }
                        layout.readd(key)?;
                    }
                    Action::Place(place) => {
                        let token = SlotToken::parse(place.slot.as_ref())?;
                        let key = RunKey::try_from(place.run.as_ref().ok_or_else(invalid)?)?;
                        token.check(layout)?;
                        if !runs
                            .as_ref()
                            .ok_or_else(connection_required)?
                            .get(&key)
                            .is_some_and(|run| run.navigable)
                        {
                            return Err(stale().into());
                        }
                        layout.place(token.id, token.revision, key)?;
                    }
                }
                if let Some(runs) = runs {
                    layout.reconcile(&runs, &BTreeSet::new())?;
                }
                Ok(Body::SlotLayoutEdited(SlotLayoutEdited {}))
            })
            .map_err(store_fault)
    }

    fn require_ready(&self) -> Result<&Remote, ProtocolFault> {
        self.generation
            .and(self.remote.as_ref())
            .ok_or_else(connection_required)
    }

    fn require_generation(&self, generation: &str) -> Result<&Remote, ProtocolFault> {
        let remote = self.require_ready()?;
        if self.generation != Some(parse_id(generation)?) {
            return Err(stale());
        }
        Ok(remote)
    }
}

#[cfg(windows)]
fn local_unavailable() -> ProtocolFault {
    ProtocolFault::new(
        "LocalTerminalUnavailable",
        "cannot verify the selected WezTerm instance and pane. Pair the intended terminal again",
    )
}

fn require_binding(
    remote: &Remote,
    binding: Option<&PresentedBinding>,
) -> Result<(Uuid, u64), ProtocolFault> {
    if remote.bindings.is_empty() {
        return Err(ProtocolFault::new(
            "NoAttachedClient",
            "pair an attached TUI first",
        ));
    }
    let binding = binding.ok_or_else(|| {
        if remote.bindings.len() > 1 {
            ProtocolFault::new("AmbiguousClient", "choose the intended TUI registration")
        } else {
            invalid()
        }
    })?;
    let client = parse_id(&binding.client_id)?;
    if remote.bindings.get(&client) != Some(&binding.revision) {
        return Err(stale());
    }
    Ok((client, binding.revision))
}

struct SlotToken {
    id: u32,
    revision: u64,
    run: Option<RunKey>,
}

impl SlotToken {
    fn parse(token: Option<&PresentedSlotToken>) -> Result<Self, ProtocolFault> {
        let token = token.ok_or_else(invalid)?;
        Ok(Self {
            id: token.slot_id,
            revision: token
                .revision
                .filter(|revision| *revision > 0)
                .ok_or_else(invalid)?,
            run: token.run.as_ref().map(RunKey::try_from).transpose()?,
        })
    }

    fn check(&self, layout: &SlotLayout) -> Result<(), ProtocolFault> {
        layout.exact_slot(self.id, self.revision, self.run)?;
        Ok(())
    }
}

fn invalid_reply() -> ProtocolFault {
    ProtocolFault::new(
        "OutcomeUnknown",
        "remote returned an unexpected command result",
    )
}

fn stale() -> ProtocolFault {
    ProtocolFault::new(
        "StaleBinding",
        "displayed slot, registration, or connection has changed",
    )
}

fn connection_required() -> ProtocolFault {
    ProtocolFault::new(
        "ConnectionRequired",
        "wait for the configured host to synchronize",
    )
}
