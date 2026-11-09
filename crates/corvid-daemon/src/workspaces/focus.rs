use std::time::Instant;

use corvid_core::InputRoute;
use corvid_core::layout::Location;
use corvid_protocol::ProtocolFault;
use corvid_protocol::wire::FocusApplied;
use corvid_protocol::wire::envelope::Body;
use tokio::sync::mpsc::error::TrySendError;
use uuid::Uuid;

use super::controllers::{Sender, no_client};
use super::{Active, Registry, Reply, ReservedSwitch, Workspaces, open_pane, target_gone};
use crate::projection::selection;
use crate::terminal::InputMode;

pub(super) struct FocusTarget {
    pub(super) controller: String,
    pub(super) client: Uuid,
    pub(super) run: Uuid,
    pub(super) revision: u64,
    pub(super) expires: Instant,
}

pub(crate) struct FocusDispatch {
    target: FocusTarget,
    receipt: Sender,
}

impl FocusDispatch {
    pub(super) fn new(target: FocusTarget, receipt: Sender) -> Self {
        Self { target, receipt }
    }

    pub(crate) fn complete(self, outcome: Reply<FocusApplied>) {
        self.receipt
            .send_replace(Some(outcome.map(Body::FocusApplied)));
    }
}

pub(crate) struct Focused {
    pub(crate) reply: FocusApplied,
    pub(crate) active: Option<Active>,
}

pub(crate) struct FocusRejected {
    pub(crate) fault: ProtocolFault,
    pub(crate) recovery: FocusRecovery,
}

pub(crate) enum FocusRecovery {
    Unchanged,
    Restored(InputRoute),
    Lost,
}

impl From<ProtocolFault> for FocusRejected {
    fn from(fault: ProtocolFault) -> Self {
        Self {
            fault,
            recovery: FocusRecovery::Unchanged,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FocusDestination {
    location: Location,
    terminal: Uuid,
    lease: u64,
}

enum Prepared {
    Current(FocusApplied),
    Move(FocusDestination, ReservedSwitch),
}

impl Workspaces {
    pub(crate) async fn focus(
        &self,
        old: &Active,
        dispatch: &FocusDispatch,
    ) -> Result<Focused, FocusRejected> {
        let target = &dispatch.target;
        let (destination, reserved) = match self.prepare_focus(old, target)? {
            Prepared::Current(reply) => {
                return Ok(Focused {
                    reply,
                    active: None,
                });
            }
            Prepared::Move(destination, reserved) => (destination, reserved),
        };
        let result = self.switch_focus(old, target, destination, reserved).await;
        if let Some(slot) = self
            .lock_registry()
            .slots
            .get_mut(&destination.location.pane)
        {
            slot.reserved = false;
        }
        result
    }

    fn prepare_focus(&self, old: &Active, target: &FocusTarget) -> Reply<Prepared> {
        let mut registry = self.lock_registry();
        let destination = registry.focus_destination(target)?;
        registry.require_route(old.route)?;
        if old.route.terminal_id == destination.terminal {
            return Ok(Prepared::Current(registry.focus_reply(target, old)?));
        }
        let reserved = self.reserve_view(
            &mut registry,
            old,
            destination.location,
            destination.lease,
            None,
        )?;
        Ok(Prepared::Move(destination, reserved))
    }

    async fn switch_focus(
        &self,
        old: &Active,
        target: &FocusTarget,
        destination: FocusDestination,
        reserved: ReservedSwitch,
    ) -> Result<Focused, FocusRejected> {
        let next = open_pane(
            reserved.terminal,
            reserved.location,
            reserved.spec,
            reserved.events,
        )
        .await?;
        if next.route.terminal_id != destination.terminal {
            return Err(target_gone().into());
        }
        target.require_time()?;
        if let Err(fault) = old.terminal.freeze(old.route).await {
            let recovery = if fault.code == "Timeout" {
                self.restore_source(old).await
            } else {
                FocusRecovery::Unchanged
            };
            return Err(FocusRejected { fault, recovery });
        }

        let committed = async {
            target.require_time()?;
            next.terminal.activate(next.route, next.viewport).await?;
            self.commit_focus(old, &next, target, destination)
        }
        .await;
        match committed {
            Ok(reply) => Ok(Focused {
                reply,
                active: Some(next),
            }),
            Err(fault) => {
                drop(next);
                let recovery = self.restore_source(old).await;
                Err(FocusRejected { fault, recovery })
            }
        }
    }

    fn commit_focus(
        &self,
        old: &Active,
        next: &Active,
        target: &FocusTarget,
        expected: FocusDestination,
    ) -> Reply<FocusApplied> {
        let mut registry = self.lock_registry();
        let destination = registry.focus_destination(target)?;
        registry.require_route(old.route)?;
        if destination != expected || next.route.writer_lease_generation != destination.lease {
            return Err(target_gone());
        }
        registry.record_view(next);
        registry.focus_reply(target, next)
    }

    async fn restore_source(&self, old: &Active) -> FocusRecovery {
        let restored = old.terminal.thaw(old.route).await;
        let mut registry = self.lock_registry();
        let Some(view) = registry
            .clients
            .get_mut(&old.route.client_id)
            .and_then(|client| client.view.as_mut())
            .filter(|view| view.route == old.route)
        else {
            return FocusRecovery::Lost;
        };
        let recovery = if let Ok(route) = restored {
            view.route = route;
            view.input_mode = InputMode::Passthrough;
            FocusRecovery::Restored(route)
        } else {
            registry
                .clients
                .get_mut(&old.route.client_id)
                .expect("client still exists")
                .view = None;
            FocusRecovery::Lost
        };
        registry.publish_client(old.route.client_id);
        recovery
    }
}

impl FocusTarget {
    fn require_time(&self) -> Reply<()> {
        if Instant::now() < self.expires {
            Ok(())
        } else {
            Err(ProtocolFault::new(
                "Timeout",
                "focus command expired before commit",
            ))
        }
    }
}

impl Registry {
    pub(super) fn dispatch_focus(&self, dispatch: FocusDispatch) {
        if let Err(fault) = self.focus_destination(&dispatch.target) {
            dispatch.complete(Err(fault));
            return;
        }
        let client = &self.clients[&dispatch.target.client];
        if let Err(error) = client.focuses.try_send(dispatch) {
            let (dispatch, fault) = match error {
                TrySendError::Full(dispatch) => (
                    dispatch,
                    ProtocolFault::new("InputBusy", "client has a pending focus command"),
                ),
                TrySendError::Closed(dispatch) => (dispatch, no_client()),
            };
            dispatch.complete(Err(fault));
        }
    }

    fn focus_destination(&self, target: &FocusTarget) -> Reply<FocusDestination> {
        target.require_time()?;
        self.require_publication()?;
        let client = self
            .clients
            .get(&target.client)
            .filter(|client| client.view.is_some() && !client.focuses.is_closed())
            .ok_or_else(no_client)?;
        self.controllers
            .verify(target.client, &target.controller, target.revision)?;

        let (terminal, owner) = self
            .agents
            .focus_terminal(target.run)
            .ok_or_else(target_gone)?;
        if !owner.is_current() {
            return Err(target_gone());
        }
        let location = *self.terminals.get(&terminal).ok_or_else(target_gone)?;
        if !self
            .slots
            .get(&location.pane)
            .is_some_and(|slot| slot.terminal.has_live_terminal())
        {
            return Err(target_gone());
        }
        let lease = self.require_writer(target.client, location.workspace)?;
        debug_assert_eq!(client.view.as_ref().unwrap().route.client_id, target.client);
        Ok(FocusDestination {
            location,
            terminal,
            lease,
        })
    }

    pub(super) fn require_route(&self, route: InputRoute) -> Reply<()> {
        if self
            .clients
            .get(&route.client_id)
            .and_then(|client| client.view.as_ref())
            .is_some_and(|view| view.route == route)
        {
            Ok(())
        } else {
            Err(no_client())
        }
    }

    fn focus_reply(&self, target: &FocusTarget, view: &Active) -> Reply<FocusApplied> {
        Ok(FocusApplied {
            client_id: target.client.to_string(),
            agent_run_id: target.run.to_string(),
            terminal_id: view.route.terminal_id.to_string(),
            selection: Some(selection(view.location)),
            view_revision: view.route.view_revision,
            commit_sequence: self.publication.sequence()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Arc;

    use corvid_core::agent::{ProcessLiveness, Provider, Source};
    use corvid_core::layout::Layout;
    use corvid_core::{KEY_LEADER, Viewport};
    use corvid_protocol::COMMAND_TIMEOUT;
    use corvid_protocol::wire::TuiInputMode;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    use super::*;
    use crate::agents::ManagedLaunch;
    use crate::collector::HookEnvironment;
    use crate::identity::Identity;
    use crate::metadata::MetadataUpdates;
    use crate::process::{self, ProcessImage};
    use crate::terminal::{Action, Event, Keymap, Launch};
    use crate::workspaces::{AttachTarget, ControllerCommand};

    fn workspaces() -> Workspaces {
        let metadata = MetadataUpdates::in_memory(Layout::empty_terminal());
        let identity = Identity {
            host_id: Uuid::new_v4().to_string(),
            epoch: Uuid::new_v4().to_string(),
        };
        Workspaces::new(
            metadata,
            Arc::new(HookEnvironment {
                runtime: PathBuf::from("/tmp"),
                identity: Arc::new(identity),
            }),
        )
    }

    async fn attach(
        workspaces: &Workspaces,
        tab: Option<&str>,
    ) -> (Active, mpsc::Receiver<FocusDispatch>) {
        let (sender, receiver) = mpsc::channel(1);
        let active = workspaces
            .attach(
                Uuid::new_v4(),
                AttachTarget {
                    workspace: None,
                    tab: None,
                    new_workspace: None,
                    new_tab: tab.map(str::to_owned),
                    keymap: Keymap::default(),
                    viewport: Viewport::new(80, 24).unwrap(),
                    launch: Some(Launch {
                        argv: ["/bin/sleep", "30"].map(OsString::from).to_vec(),
                        cwd: None,
                        provider: None,
                        if_empty: false,
                    }),
                },
                sender,
                None,
            )
            .await
            .unwrap();
        (active, receiver)
    }

    #[tokio::test]
    async fn revoking_binding_after_fence_restores_source_with_a_new_input_token() {
        let workspaces = workspaces();
        let (target, _target_mailbox) = attach(&workspaces, None).await;
        let managed = ManagedLaunch::reserve(
            &Arc::downgrade(&workspaces.registry),
            target.location,
            target.route.terminal_id,
            Provider::Claude,
            false,
        )
        .unwrap();
        let incarnation = process::incarnation(std::process::id(), process::boot_id().unwrap());
        let owner = incarnation.and_then(|incarnation| ProcessImage::capture(incarnation, None));
        managed.started(
            ProcessLiveness::Running,
            Source::ProcessProbe,
            incarnation,
            owner,
        );
        let run = Uuid::parse_str(&workspaces.agents().agents[0].agent_run_id).unwrap();
        workspaces.release(target.route.client_id);
        drop(target);

        let (mut old, _source_mailbox) = attach(&workspaces, Some("Source")).await;
        let client = old.route.client_id;
        let offer = workspaces
            .issue_pairing_code(client, old.route.connection_generation)
            .unwrap();
        let code = Uuid::parse_str(&offer.code).unwrap();
        let Body::ClientBindingRegistered(binding) = workspaces
            .controller_command("controller", "pair", ControllerCommand::Register { code })
            .await
            .unwrap()
        else {
            panic!("missing binding")
        };
        let target = FocusTarget {
            controller: "controller".to_owned(),
            client,
            run,
            revision: binding.binding_revision,
            expires: Instant::now() + COMMAND_TIMEOUT,
        };

        let Prepared::Move(destination, reserved) =
            workspaces.prepare_focus(&old, &target).unwrap()
        else {
            panic!("focus must prepare a distinct view");
        };
        let next = open_pane(
            reserved.terminal,
            reserved.location,
            reserved.spec,
            reserved.events,
        )
        .await
        .unwrap();
        old.terminal
            .action(
                client,
                None,
                Action::Input {
                    route: old.route,
                    sequence: 1,
                    bytes: vec![KEY_LEADER],
                },
            )
            .await
            .unwrap();
        old.terminal.freeze(old.route).await.unwrap();
        assert_eq!(
            workspaces.clients().clients[0].input_mode,
            Some(TuiInputMode::Command as i32)
        );
        next.terminal
            .activate(next.route, next.viewport)
            .await
            .unwrap();
        workspaces
            .controller_command(
                "controller",
                "unpair",
                ControllerCommand::Unregister {
                    client,
                    revision: binding.binding_revision,
                },
            )
            .await
            .unwrap();

        let fault = workspaces
            .commit_focus(&old, &next, &target, destination)
            .unwrap_err();
        assert_eq!(fault.code, "StaleBinding");
        drop(next);
        let FocusRecovery::Restored(restored) = workspaces.restore_source(&old).await else {
            panic!("source was not restored");
        };
        assert_eq!(restored.terminal_id, old.route.terminal_id);
        assert_eq!(restored.view_revision, old.route.view_revision + 1);
        let client_state = workspaces.clients().clients.pop().unwrap();
        assert_eq!(client_state.view_revision, Some(restored.view_revision));
        assert_eq!(
            client_state.input_mode,
            Some(TuiInputMode::Passthrough as i32)
        );
        assert_eq!(
            client_state.terminal_id.as_deref(),
            Some(restored.terminal_id.to_string().as_str())
        );

        old.terminal
            .action(
                client,
                None,
                Action::Input {
                    route: old.route,
                    sequence: 2,
                    bytes: b"stale".to_vec(),
                },
            )
            .await
            .unwrap();
        timeout(COMMAND_TIMEOUT, async {
            loop {
                if let Event::Error { fault, .. } =
                    old.events.recv().await.expect("source view closed")
                {
                    assert_eq!(fault.code, "StaleRoute");
                    break;
                }
            }
        })
        .await
        .unwrap();
        workspaces.release(client);
        drop(old);
        drop(managed);
        workspaces.shutdown().await.unwrap();
    }
}
