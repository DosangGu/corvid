use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use corvid_protocol::controller::PAIRING_SECONDS;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AttentionAcknowledged, ClientBindingRegistered, ClientBindingState, ClientBindingUnregistered,
    PairingCode,
};
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault, Session};
use tokio::sync::watch;
use tokio::time::timeout;
use uuid::Uuid;

use super::focus::{FocusDispatch, FocusTarget};
use super::{Registry, Reply, Workspaces};

const PAIR_LIFETIME: Duration = Duration::from_secs(PAIRING_SECONDS as u64);
const GUESS_WINDOW: Duration = Duration::from_secs(10);
const MAX_GUESSES: usize = 12;
const MAX_BINDINGS_PER_CLIENT: usize = 8;
const RPC_HISTORY: usize = 4096;
const RPC_LIFETIME: Duration = Duration::from_secs(600);

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ControllerCommand {
    Register {
        code: Uuid,
    },
    Unregister {
        client: Uuid,
        revision: u64,
    },
    Acknowledge {
        run: Uuid,
        attention: Uuid,
    },
    Focus {
        client: Uuid,
        run: Uuid,
        revision: u64,
    },
}

impl ControllerCommand {
    pub(crate) fn parse(session: &Session, body: &Body) -> Reply<Self> {
        let (controller, command) = match body {
            Body::RegisterClientBinding(binding) => (
                &binding.controller_id,
                Self::Register {
                    code: parse_uuid(&binding.code)?,
                },
            ),
            Body::UnregisterClientBinding(binding) => (
                &binding.controller_id,
                Self::Unregister {
                    client: parse_uuid(&binding.client_id)?,
                    revision: binding_revision(binding.binding_revision)?,
                },
            ),
            Body::AcknowledgeAttention(ack) => (
                &ack.controller_id,
                Self::Acknowledge {
                    run: parse_uuid(&ack.agent_run_id)?,
                    attention: parse_uuid(&ack.attention_id)?,
                },
            ),
            Body::FocusAgent(focus) => (
                &focus.controller_id,
                Self::Focus {
                    client: parse_uuid(&focus.client_id)?,
                    run: parse_uuid(&focus.agent_run_id)?,
                    revision: binding_revision(focus.binding_revision)?,
                },
            ),
            _ => return Err(invalid("unknown controller command")),
        };
        if session.controller_id.as_deref() != Some(controller) {
            return Err(ProtocolFault::new(
                "Unauthorized",
                "controller does not match Hello",
            ));
        }
        Ok(command)
    }
}

#[derive(Default)]
pub(super) struct Controllers {
    offers: BTreeMap<Uuid, Offer>,
    bindings: BTreeMap<Uuid, BTreeMap<String, u64>>,
    revision: u64,
    guesses: VecDeque<Instant>,
    journal: Journal,
}

struct Offer {
    code: Uuid,
    generation: u64,
    created: Instant,
}

impl Controllers {
    pub(super) fn bindings(&self, client: Uuid) -> Vec<ClientBindingState> {
        self.bindings
            .get(&client)
            .into_iter()
            .flat_map(|controllers| controllers.iter())
            .map(|(controller, revision)| ClientBindingState {
                controller_id: controller.clone(),
                binding_revision: *revision,
            })
            .collect()
    }

    pub(super) fn disconnect(&mut self, client: Uuid) {
        self.offers.remove(&client);
        self.bindings.remove(&client);
    }

    fn offer(&mut self, client: Uuid, generation: u64, now: Instant) -> Reply<PairingCode> {
        if self
            .offers
            .get(&client)
            .is_some_and(|offer| now.duration_since(offer.created) < Duration::from_secs(1))
        {
            return Err(ProtocolFault::new(
                "InputBusy",
                "pairing code was just issued",
            ));
        }
        let code = Uuid::new_v4();
        self.offers.insert(
            client,
            Offer {
                code,
                generation,
                created: now,
            },
        );
        Ok(PairingCode {
            code: code.to_string(),
            expires_in_seconds: PAIRING_SECONDS,
        })
    }

    fn attempt(&mut self, code: Uuid, now: Instant) -> Reply<(Uuid, u64)> {
        self.guesses
            .retain(|attempt| now.duration_since(*attempt) < GUESS_WINDOW);
        if self.guesses.len() >= MAX_GUESSES {
            return Err(ProtocolFault::new(
                "LimitExceeded",
                "pairing attempts are temporarily limited",
            ));
        }
        self.guesses.push_back(now);
        self.offers
            .retain(|_, offer| now.duration_since(offer.created) < PAIR_LIFETIME);
        self.offers
            .iter()
            .find_map(|(client, offer)| (offer.code == code).then_some((*client, offer.generation)))
            .ok_or_else(|| ProtocolFault::new("Unauthorized", "pairing code is unavailable"))
    }

    fn register(&mut self, client: Uuid, controller: &str) -> Reply<u64> {
        let bindings = self.bindings.entry(client).or_default();
        if bindings.len() >= MAX_BINDINGS_PER_CLIENT && !bindings.contains_key(controller) {
            return Err(ProtocolFault::new(
                "LimitExceeded",
                "client binding capacity reached",
            ));
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| ProtocolFault::new("LimitExceeded", "binding revisions exhausted"))?;
        self.revision = revision;
        bindings.insert(controller.to_owned(), revision);
        self.offers.remove(&client);
        Ok(revision)
    }

    fn unregister(&mut self, client: Uuid, controller: &str, revision: u64) -> Reply<()> {
        self.verify(client, controller, revision)?;
        let bindings = self.bindings.get_mut(&client).ok_or_else(stale_binding)?;
        bindings.remove(controller);
        Ok(())
    }

    pub(super) fn verify(&self, client: Uuid, controller: &str, revision: u64) -> Reply<()> {
        if self
            .bindings
            .get(&client)
            .and_then(|bindings| bindings.get(controller))
            == Some(&revision)
        {
            Ok(())
        } else {
            Err(stale_binding())
        }
    }
}

#[derive(Default)]
struct Journal {
    entries: BTreeMap<RpcKey, Entry>,
    order: VecDeque<RpcKey>,
}

type RpcKey = (String, String);
type Outcome = Reply<Body>;
pub(super) type Sender = watch::Sender<Option<Outcome>>;
type Receipt = watch::Receiver<Option<Outcome>>;

struct Entry {
    created: Instant,
    command: ControllerCommand,
    receipt: Receipt,
}

impl Journal {
    fn begin(
        &mut self,
        controller: &str,
        request: &str,
        command: &ControllerCommand,
        now: Instant,
    ) -> Reply<(Receipt, Option<Sender>)> {
        while let Some(oldest) = self.order.front() {
            if now.duration_since(self.entries[oldest].created) < RPC_LIFETIME {
                break;
            }
            self.entries.remove(oldest);
            self.order.pop_front();
        }
        let key = (controller.to_owned(), request.to_owned());
        if let Some(entry) = self.entries.get(&key) {
            if entry.command != *command {
                return Err(invalid("request ID was reused for another command"));
            }
            return Ok((entry.receipt.clone(), None));
        }
        if self.entries.len() == RPC_HISTORY {
            return Err(ProtocolFault::new(
                "LimitExceeded",
                "controller request history is full",
            ));
        }
        let (sender, receipt) = watch::channel(None);
        self.order.push_back(key.clone());
        self.entries.insert(
            key,
            Entry {
                created: now,
                command: command.clone(),
                receipt: receipt.clone(),
            },
        );
        Ok((receipt, Some(sender)))
    }
}

impl Workspaces {
    pub(crate) fn issue_pairing_code(&self, client: Uuid, generation: u64) -> Reply<PairingCode> {
        let mut registry = self.lock_registry();
        registry.require_attached(client, generation)?;
        registry
            .controllers
            .offer(client, generation, Instant::now())
    }

    pub(crate) async fn controller_command(
        &self,
        controller: &str,
        request: &str,
        command: ControllerCommand,
    ) -> Outcome {
        let receipt = {
            let mut registry = self.lock_registry();
            let now = Instant::now();
            let (receipt, sender) = registry
                .controllers
                .journal
                .begin(controller, request, &command, now)?;
            if let Some(sender) = sender {
                if let ControllerCommand::Focus {
                    client,
                    run,
                    revision,
                } = command
                {
                    let target = FocusTarget {
                        client,
                        run,
                        authorization: super::focus::FocusAuthorization::Binding {
                            controller: controller.to_owned(),
                            revision,
                        },
                        expires: now + COMMAND_TIMEOUT,
                    };
                    registry.dispatch_focus(FocusDispatch::new(target, sender));
                } else {
                    let outcome = registry.execute_controller(controller, command);
                    sender.send_replace(Some(outcome));
                }
            }
            receipt
        };
        timeout(COMMAND_TIMEOUT, wait_receipt(receipt))
            .await
            .map_err(|_| {
                ProtocolFault::new("Timeout", "controller command result is unavailable")
            })?
    }
}

async fn wait_receipt(mut receipt: Receipt) -> Outcome {
    loop {
        if let Some(outcome) = receipt.borrow_and_update().clone() {
            return outcome;
        }
        receipt.changed().await.map_err(|_| no_client())?;
    }
}

impl Registry {
    fn execute_controller(&mut self, controller: &str, command: ControllerCommand) -> Outcome {
        self.require_publication()?;
        match command {
            ControllerCommand::Register { code } => {
                let (client, generation) = self.controllers.attempt(code, Instant::now())?;
                self.require_attached(client, generation)?;
                let revision = self.controllers.register(client, controller)?;
                self.publish_client(client);
                Ok(Body::ClientBindingRegistered(ClientBindingRegistered {
                    client_id: client.to_string(),
                    binding_revision: revision,
                    commit_sequence: self.publication.sequence()?,
                }))
            }
            ControllerCommand::Unregister { client, revision } => {
                self.controllers.unregister(client, controller, revision)?;
                self.publish_client(client);
                Ok(Body::ClientBindingUnregistered(ClientBindingUnregistered {
                    commit_sequence: self.publication.sequence()?,
                }))
            }
            ControllerCommand::Acknowledge { run, attention } => {
                let acknowledged = self
                    .agents
                    .acknowledge(run, attention)
                    .ok_or_else(super::target_gone)?;
                self.publish(Vec::new());
                Ok(Body::AttentionAcknowledged(AttentionAcknowledged {
                    agent_run_id: run.to_string(),
                    attention_id: attention.to_string(),
                    acknowledged_unix_ms: acknowledged,
                    commit_sequence: self.publication.sequence()?,
                }))
            }
            ControllerCommand::Focus { .. } => unreachable!("focus is dispatched to the TUI"),
        }
    }

    fn require_attached(&self, client: Uuid, generation: u64) -> Reply<()> {
        if self
            .clients
            .get(&client)
            .is_some_and(|client| client.generation == generation && client.view.is_some())
        {
            Ok(())
        } else {
            Err(no_client())
        }
    }

    pub(super) fn require_publication(&self) -> Reply<()> {
        if self.publication.sequence()? == u64::MAX {
            Err(ProtocolFault::new(
                "LimitExceeded",
                "publication sequence exhausted",
            ))
        } else {
            Ok(())
        }
    }
}

fn parse_uuid(value: &str) -> Reply<Uuid> {
    Uuid::parse_str(value)
        .ok()
        .filter(|id| !id.is_nil())
        .ok_or_else(|| invalid("invalid controller command ID"))
}

fn binding_revision(revision: Option<u64>) -> Reply<u64> {
    revision
        .filter(|revision| *revision > 0)
        .ok_or_else(|| invalid("missing binding revision"))
}

fn invalid(message: &str) -> ProtocolFault {
    ProtocolFault::new("InvalidMessage", message)
}

fn stale_binding() -> ProtocolFault {
    ProtocolFault::new("StaleBinding", "controller binding is absent or changed")
}

pub(super) fn no_client() -> ProtocolFault {
    ProtocolFault::new("NoAttachedClient", "requested TUI client is unavailable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_offer_expires_and_guesses_share_a_host_budget() {
        let mut state = Controllers::default();
        let now = Instant::now();
        let client = Uuid::new_v4();
        let offer = state.offer(client, 7, now).unwrap();
        let code = Uuid::parse_str(&offer.code).unwrap();
        assert_eq!(
            state.attempt(code, now + PAIR_LIFETIME).unwrap_err().code,
            "Unauthorized"
        );

        let later = now + PAIR_LIFETIME + GUESS_WINDOW;
        for _ in 0..MAX_GUESSES {
            assert_eq!(
                state.attempt(Uuid::new_v4(), later).unwrap_err().code,
                "Unauthorized"
            );
        }
        assert_eq!(
            state.attempt(Uuid::new_v4(), later).unwrap_err().code,
            "LimitExceeded"
        );
        let _new_offer = state.offer(client, 7, later).unwrap();
        assert_eq!(
            state
                .attempt(Uuid::new_v4(), later + GUESS_WINDOW)
                .unwrap_err()
                .code,
            "Unauthorized"
        );
    }
}
