mod actor;
mod close;
mod controllers;
mod focus;
mod restore;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod shell;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::executor::time::timeout;
use crate::executor::{Task, blocking};
use anyhow::Result;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use corvid_core::agent::ProcessLiveness;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use corvid_core::agent::Provider;
use corvid_core::layout::{Axis, CloseTarget, CwdObservation, CwdSource, Layout, Location};
use corvid_core::{InputRoute, Viewport};
use corvid_protocol::wire::snapshot_entity::Body as EntityBody;
use corvid_protocol::wire::{
    AgentList, ClientKeymap, ClientList, ClientState, CollectorStatus, PaneWriterView, TabMoved,
    TerminalState, TuiInputMode, Viewport as WireViewport, WriterLeaseState,
};
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::agents::Agents;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::agents::{Hook, HookBinding};
use crate::collector::{CollectorDiagnostics, HookEnvironment};
use crate::metadata::MetadataUpdates;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::process::{self, HookProducer, Incarnation, ProbeBudget, ProcessImage};
use crate::projection;
use crate::publication::{Capture, Group, Projection, Publication};
use crate::terminal::{
    AttachSpec, Event, HostConfig, InputMode, Keymap, Launch, LaunchScope, RetainedTerminal,
    TabCommand, TerminalHandle, TuiViewport,
};

use actor::RegistryActor;
pub(crate) use actor::WeakRegistry;
pub(crate) use close::Closed;
pub(crate) use controllers::ControllerCommand;
use controllers::Controllers;
pub(crate) use focus::{FocusDispatch, FocusRecovery};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use shell::Shells;

pub(crate) const MAX_PTY_WORKERS: usize = 32;
const EVENT_CAPACITY: usize = 4;

type Reply<T> = Result<T, ProtocolFault>;

pub(crate) struct AttachTarget {
    pub(crate) agent_run: Option<Uuid>,
    pub(crate) workspace: Option<Uuid>,
    pub(crate) tab: Option<Uuid>,
    pub(crate) pane: Option<Uuid>,
    pub(crate) new_workspace: Option<String>,
    pub(crate) workspace_directory: Option<Vec<u8>>,
    pub(crate) fallback_directory: Option<PathBuf>,
    pub(crate) new_tab: Option<String>,
    pub(crate) keymap: Keymap,
    pub(crate) viewport: TuiViewport,
    pub(crate) launch: Option<Launch>,
    pub(crate) take_over: bool,
    pub(crate) expected_terminal: Option<Uuid>,
    pub(crate) resume_token: Option<Uuid>,
    pub(crate) issue_resume_token: bool,
    pub(crate) offer_recovery: bool,
}

#[derive(Debug)]
pub(crate) enum AttachFailure {
    Fault(ProtocolFault),
    Recovery(Box<corvid_protocol::wire::PaneRestoreResult>),
}

impl From<ProtocolFault> for AttachFailure {
    fn from(fault: ProtocolFault) -> Self {
        Self::Fault(fault)
    }
}

pub(crate) struct Active {
    pub(crate) location: Location,
    pub(crate) viewport: TuiViewport,
    pub(crate) route: InputRoute,
    pub(crate) terminal: TerminalHandle,
    pub(crate) events: mpsc::Receiver<Event>,
    pub(crate) resume_token: Option<Uuid>,
}

#[derive(Clone, Copy)]
struct View {
    location: Location,
    viewport: TuiViewport,
    route: InputRoute,
}

impl Active {
    fn view(&self) -> View {
        View {
            location: self.location,
            viewport: self.viewport,
            route: self.route,
        }
    }
}

#[derive(Clone)]
pub(crate) enum Destination {
    Exact(Location),
    Open(Location),
    CreateWorkspace(String, Option<Vec<u8>>),
    CreateTab(String),
    Split(Axis),
    Tab(TabCommand),
}

pub(crate) struct Switched {
    pub(crate) active: Active,
    pub(crate) paste_discarded: bool,
}

struct ReservedAttach {
    location: Location,
    generation: u64,
    terminal: TerminalHandle,
    previous: Option<InputRoute>,
    resume_token: Option<Uuid>,
}

struct ReservedSwitch {
    location: Location,
    terminal: TerminalHandle,
    spec: AttachSpec,
    events: mpsc::Receiver<Event>,
}

#[derive(Clone)]
pub(crate) struct Workspaces {
    pub(crate) collector: Arc<CollectorDiagnostics>,
    registry: RegistryActor,
    metadata: MetadataUpdates,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    boot_id: Option<Uuid>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    probes: Arc<ProbeBudget>,
    hook_environment: Arc<HookEnvironment>,
    host: Option<HostConfig>,
}

pub(crate) struct Registry {
    pub(crate) agents: Agents,
    metadata: MetadataUpdates,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    epoch: Uuid,
    publication: Publication,
    titles: Arc<HashMap<Uuid, String>>,
    terminals: HashMap<Uuid, Location>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    shells: Shells,
    clients: HashMap<Uuid, Client>,
    controllers: Controllers,
    generation: u64,
    claims: HashMap<Uuid, Claim>,
    pane_claims: HashMap<Uuid, Claim>,
    closing: Vec<CloseTarget>,
    slots: HashMap<Uuid, Slot>,
    retired: Vec<Task>,
    restores: HashMap<Uuid, restore::Attempt>,
}

struct Client {
    generation: u64,
    keymap: Keymap,
    resume_token: Option<Uuid>,
    view: Option<ClientView>,
    focuses: mpsc::Sender<FocusDispatch>,
    disconnect: Option<oneshot::Sender<()>>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    notices: Option<mpsc::Sender<Provider>>,
}

struct ClientView {
    route: InputRoute,
    location: Location,
    viewport: Viewport,
    input_mode: InputMode,
}

#[derive(Clone, Copy)]
struct Claim {
    client: Uuid,
    generation: u64,
}

struct Slot {
    workspace: Uuid,
    terminal: TerminalHandle,
    task: Task,
    reserved: bool,
    // Keep the last writer's token while this PTY is detached so a lost attach
    // reply can be retried with the same token.
    resume_token: Option<(Uuid, Uuid)>,
}

impl Workspaces {
    pub(crate) async fn rename_tab(&self, active: &Active, name: String) -> Reply<()> {
        let active = active.view();
        self.registry
            .request(move |registry| {
                registry.require_pane_writer(active.route.client_id, active.location)?;
                if !registry
                    .clients
                    .get(&active.route.client_id)
                    .and_then(|client| client.view.as_ref())
                    .is_some_and(|view| view.route == active.route)
                {
                    return Err(target_gone());
                }
                let mut layout = registry.metadata.layout();
                Arc::make_mut(&mut layout)
                    .rename_tab(active.location, (!name.is_empty()).then_some(name))
                    .map_err(|_| ProtocolFault::new("InvalidMessage", "invalid view name"))?;
                registry.publish_layout(layout, Vec::new());
                Ok(())
            })
            .await
    }
    pub(crate) fn new(metadata: MetadataUpdates, hook_environment: Arc<HookEnvironment>) -> Self {
        let publication = Publication::seed(projection::layout(&metadata.layout()));
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let epoch = Uuid::parse_str(&hook_environment.identity.epoch).expect("valid daemon epoch");
        Self {
            collector: Arc::default(),
            registry: RegistryActor::start(Registry {
                agents: Agents::default(),
                metadata: metadata.clone(),
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                epoch,
                publication,
                titles: Arc::default(),
                terminals: HashMap::new(),
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                shells: Shells::default(),
                clients: HashMap::new(),
                controllers: Controllers::default(),
                generation: 0,
                claims: HashMap::new(),
                pane_claims: HashMap::new(),
                closing: Vec::new(),
                slots: HashMap::new(),
                retired: Vec::new(),
                restores: HashMap::new(),
            }),
            metadata,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            boot_id: process::boot_id(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            probes: Arc::new(ProbeBudget::default()),
            hook_environment,
            host: None,
        }
    }

    pub(crate) fn enable_terminal_hosts(mut self, runtime: &Path) -> Result<Self> {
        self.host = Some(HostConfig::new(runtime)?);
        Ok(self)
    }

    pub(crate) async fn is_idle(&self) -> bool {
        self.registry
            .request(move |registry| {
                registry.sweep();
                registry.clients.is_empty()
                    && registry.claims.is_empty()
                    && registry.pane_claims.is_empty()
                    && registry.slots.is_empty()
                    && registry.retired.is_empty()
            })
            .await
    }

    pub(crate) async fn retain_terminals(
        &self,
        retain: bool,
    ) -> Result<Vec<(Location, RetainedTerminal)>> {
        let terminals: Vec<_> = self
            .registry
            .request(|registry| {
                anyhow::ensure!(
                    !registry.slots.values().any(|slot| slot.reserved),
                    "terminal restoration or transition is in progress"
                );
                Ok(registry
                    .slots
                    .iter()
                    .filter_map(|(pane, slot)| {
                        registry.terminals.iter().find_map(|(_, location)| {
                            (location.pane == *pane).then(|| (*location, slot.terminal.clone()))
                        })
                    })
                    .collect())
            })
            .await?;
        let mut saved = Vec::new();
        for (location, terminal) in &terminals {
            match terminal.retain(retain).await {
                Ok(Some(terminal)) => saved.push((*location, terminal)),
                Ok(None) => {}
                Err(error) => {
                    if retain {
                        for (_, terminal) in &terminals {
                            let _ = terminal.retain(false).await;
                        }
                    }
                    return Err(error.into());
                }
            }
        }
        Ok(saved)
    }

    pub(crate) async fn recover_terminals(
        &self,
        terminals: Vec<(Location, RetainedTerminal)>,
    ) -> Result<()> {
        for (location, saved) in terminals {
            anyhow::ensure!(
                self.metadata.layout().contains(location),
                "handoff pane is absent from the saved layout"
            );
            let recovered = TerminalHandle::recover(self.launch_scope(location), saved).await;
            let (terminal, task) = match recovered {
                Ok(recovered) => recovered,
                Err(error) if terminal_host_ended(&error) => continue,
                Err(error) => return Err(error),
            };
            self.registry
                .request(move |registry| {
                    registry.slots.insert(
                        location.pane,
                        Slot {
                            workspace: location.workspace,
                            terminal,
                            task,
                            reserved: false,
                            resume_token: None,
                        },
                    )
                })
                .await;
        }
        Ok(())
    }

    fn launch_scope(&self, location: Location) -> LaunchScope {
        LaunchScope {
            location,
            metadata: self.metadata.clone(),
            registry: self.registry.downgrade(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            boot_id: self.boot_id,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            probes: Arc::clone(&self.probes),
            hook_environment: Arc::clone(&self.hook_environment),
            host: self.host.clone(),
        }
    }

    async fn transaction<T: Send + 'static>(
        &self,
        apply: impl FnOnce(&mut Registry, &Self) -> T + Send + 'static,
    ) -> T {
        let workspaces = self.clone();
        self.registry
            .request(move |registry| apply(registry, &workspaces))
            .await
    }

    pub(crate) async fn agents(&self) -> AgentList {
        self.registry
            .request(|registry| registry.agents.inventory())
            .await
    }

    #[cfg(windows)]
    pub(crate) async fn tick_native_agents(&self) {
        let roots = self
            .registry
            .request(|registry| registry.agents.roots())
            .await;
        let found = if roots.is_empty() {
            Vec::new()
        } else {
            blocking(move || crate::native_process::scan(&roots))
                .await
                .unwrap_or_default()
        };
        self.registry
            .request(move |registry| {
                let Registry {
                    agents, terminals, ..
                } = registry;
                if agents.sample_shells(found, terminals) {
                    registry.publish(Vec::new());
                }
            })
            .await;
    }

    #[cfg(windows)]
    pub(crate) async fn apply_native_hook(
        &self,
        hook: crate::hook_data::Hook,
        pid: u32,
    ) -> Reply<()> {
        if hook
            .observer_epoch
            .is_some_and(|epoch| epoch.to_string() != self.hook_environment.identity.epoch)
        {
            return Err(ProtocolFault::new(
                "EpochMismatch",
                "observer launch epoch was not retained",
            ));
        }
        let unauthorized =
            || ProtocolFault::new("Unauthorized", "hook has no verified Windows agent owner");
        let association = hook.association;
        let root = self
            .registry
            .request(move |registry| registry.agents.hook_root(association))
            .await
            .ok_or_else(unauthorized)?;
        let provider = hook.provider;
        let owner = blocking(move || crate::native_process::verify_hook(pid, root, provider))
            .await
            .ok()
            .flatten()
            .ok_or_else(unauthorized)?;
        self.registry
            .request(move |registry| {
                let location = *registry
                    .terminals
                    .get(&hook.association)
                    .ok_or_else(unauthorized)?;
                let result = registry.agents.apply_hook(hook, owner, location);
                registry.publish(Vec::new());
                result
            })
            .await
    }

    pub(crate) async fn clients(&self) -> ClientList {
        self.registry
            .request(move |registry| {
                let mut clients: Vec<_> = registry
                    .clients
                    .keys()
                    .filter_map(|id| registry.client_state(*id))
                    .collect();
                clients.sort_by(|first, second| first.client_id.cmp(&second.client_id));
                ClientList { clients }
            })
            .await
    }

    pub(crate) async fn snapshot(&self, watch: bool) -> Reply<Capture> {
        self.registry
            .request(move |registry| registry.publication.capture(watch))
            .await
    }

    pub(crate) async fn public_sequence(&self) -> Reply<u64> {
        self.registry
            .request(|registry| registry.publication.sequence())
            .await
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) async fn tick_agents(&self) {
        self.poll_shell_agents().await;
        self.registry
            .request(|registry| {
                registry.agents.tick();
                registry.publish(Vec::new());
            })
            .await;
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) async fn hook_binding(&self, association: Uuid) -> HookBinding {
        self.registry
            .request(move |registry| registry.agents.binding(association))
            .await
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) async fn apply_hook(
        &self,
        hook: Hook,
        previous: ProcessImage,
        owner: ProcessImage,
        producer: HookProducer,
    ) -> Reply<()> {
        self.transaction(move |registry, workspaces| {
            let current =
                Uuid::parse_str(&workspaces.hook_environment.identity.epoch).expect("daemon epoch");
            if !registry.agents.accepts_observer_epoch(
                hook.association,
                hook.observer_epoch.unwrap_or(current),
                current,
            ) {
                return Err(ProtocolFault::new(
                    "EpochMismatch",
                    "observer launch epoch was not retained",
                ));
            }
            let notice = if owner == previous {
                registry.agents.apply_hook(hook, owner, producer)?
            } else {
                let association = hook.association;
                let terminal = registry
                    .agents
                    .restart_hook(hook, previous, owner, producer)?;
                registry.shells.restarted(terminal, association, owner);
                None
            };
            registry.publish(Vec::new());
            if let Some((location, provider)) = notice
                && let Some(claim) = registry.writer_at(location)
                && let Some(client) = registry.clients.get(&claim.client)
                && client.view.is_some()
                && let Some(notices) = &client.notices
            {
                let _ = notices.try_send(provider);
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn runtime_status(&self) -> (bool, CollectorStatus) {
        self.transaction(move |registry, workspaces| {
            let terminal_live = registry
                .slots
                .values()
                .any(|slot| slot.terminal.has_live_terminal());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let mut collector = registry.agents.collector_status();
            #[cfg(windows)]
            let mut collector = CollectorStatus::default();
            collector.traffic = Some(workspaces.collector.traffic());
            (terminal_live, collector)
        })
        .await
    }

    pub(crate) async fn attach(
        &self,
        client: Uuid,
        mut target: AttachTarget,
        focuses: mpsc::Sender<FocusDispatch>,
        disconnect: Option<oneshot::Sender<()>>,
        #[cfg(any(target_os = "linux", target_os = "macos"))] notices: Option<
            mpsc::Sender<Provider>,
        >,
    ) -> Result<Active, AttachFailure> {
        let mut checked = None;
        let ReservedAttach {
            location,
            generation,
            terminal,
            previous,
            resume_token,
        } = loop {
            let validation = checked.clone();
            let (original, reservation) = self
                .transaction(move |registry, workspaces| {
                    let reserved =
                        workspaces.reserve(registry, client, &mut target, validation.as_deref());
                    (target, reserved)
                })
                .await;
            target = original;
            match reservation {
                Ok(reserved) => break reserved,
                Err(ReservationError::Fault(fault)) => return Err(fault.into()),
                Err(ReservationError::Recovery(pane)) => return Err(AttachFailure::Recovery(pane)),
                Err(ReservationError::Check(paths)) => {
                    checked = Some(Arc::new(paths.validate().await?))
                }
            }
        };
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let spec = AttachSpec {
            location,
            client_id: client,
            generation,
            lease_generation: generation,
            revision: 1,
            dormant: false,
            keymap: target.keymap,
            viewport: target.viewport,
            launch: target.launch,
            events,
        };
        let opened = if let Some(route) = previous {
            terminal.transfer(route, spec).await.map(|(route, events)| {
                (
                    Active {
                        location,
                        viewport: target.viewport,
                        route,
                        terminal,
                        events: receiver,
                        resume_token: None,
                    },
                    Some(events),
                )
            })
        } else {
            open_pane(terminal, location, spec, receiver)
                .await
                .map(|active| (active, None))
        };

        let outcome = self
            .registry
            .request(move |registry| {
                if let Some(slot) = registry.slots.get_mut(&location.pane) {
                    slot.reserved = false;
                }
                match opened {
                    Ok((active, _))
                        if target.agent_run.is_some_and(|run| {
                            !registry
                                .agent_terminal(run)
                                .is_ok_and(|found| found == (location, active.route.terminal_id))
                        }) =>
                    {
                        Ok((active, true))
                    }
                    Ok((mut active, _previous_events)) => {
                        active.resume_token = resume_token;
                        if let Some(previous) = previous {
                            registry.pane_claims.remove(&location.pane);
                            registry.claims.remove(&location.workspace);
                            registry.insert_claim(location, target.keymap, client, generation);
                            if let Some(client) = registry.clients.get_mut(&previous.client_id) {
                                client.view = None;
                                disconnect_client(&mut client.disconnect);
                            }
                        }
                        registry.resize_hidden(
                            client,
                            active.viewport.content(),
                            Some(active.location.pane),
                        );
                        registry.clients.insert(
                            client,
                            Client {
                                generation,
                                keymap: target.keymap,
                                resume_token,
                                view: None,
                                focuses,
                                disconnect,
                                #[cfg(any(target_os = "linux", target_os = "macos"))]
                                notices,
                            },
                        );
                        if let Some(previous) = previous {
                            registry.set_view(&active.view());
                            let projections = vec![
                                registry.client_projection(previous.client_id),
                                registry.client_projection(client),
                            ];
                            registry.publish(projections);
                        } else {
                            registry.record_view(&active.view());
                        }
                        Ok((active, false))
                    }
                    Err(fault) => {
                        registry.release_claims(client);
                        if target.resume_token.is_some()
                            && matches!(
                                fault.code.as_str(),
                                "StaleRoute" | "InputBusy" | "WorkspaceBusy"
                            )
                        {
                            return Err(resume_pending());
                        }
                        Err(fault)
                    }
                }
            })
            .await?;
        let (stale, changed) = outcome;
        if !changed {
            return Ok(stale);
        }
        let _ = stale.terminal.retire_view(stale.route).await;
        self.release(client).await;
        Err(target_gone().into())
    }

    fn reserve(
        &self,
        registry: &mut Registry,
        client: Uuid,
        target: &mut AttachTarget,
        checked: Option<&CheckedCwd>,
    ) -> Reservation<ReservedAttach> {
        registry.sweep();
        let mut layout = self.metadata.layout();
        let mut location = if let Some(run) = target.agent_run {
            registry.agent_terminal(run)?.0
        } else {
            resolve(
                registry,
                &mut layout,
                target,
                target.keymap.pane_writer && !target.take_over,
            )?
        };
        if target.take_over
            && target.tab.is_none()
            && target.pane.is_none()
            && let Some(view) = registry.writer_view(location.workspace)
        {
            location = view.location;
        }
        if let Some(terminal) = target.expected_terminal
            && registry.terminals.get(&terminal) != Some(&location)
        {
            return Err(
                ProtocolFault::new("TargetGone", "the selected terminal has changed").into(),
            );
        }
        let resume_token = registry
            .slots
            .get(&location.pane)
            .and_then(|slot| slot.resume_token)
            .filter(|(terminal, token)| {
                Some(*terminal) == target.expected_terminal && Some(*token) == target.resume_token
            })
            .map(|(_, token)| token);
        let previous = if target.resume_token.is_some() {
            registry.resume_source(location, resume_token)?
        } else if target.take_over {
            registry.takeover_source(location)?
        } else {
            None
        };
        if previous.is_none() {
            registry.check_attach(location, target.keymap)?;
        }
        if target.offer_recovery
            && let Some(pane) = registry.attach_recovery(location)
        {
            return Err(ReservationError::Recovery(Box::new(pane)));
        }
        if let Some(launch) = &mut target.launch
            && (!launch.if_empty || !registry.has_live_at(location))
        {
            let paths =
                CwdPaths::for_launch(&layout, location, launch, target.fallback_directory.clone());
            launch.cwd = paths.resolved(checked)?;
        }
        let generation = registry.next_generation()?;
        let resume_token = target
            .issue_resume_token
            .then(|| resume_token.unwrap_or_else(Uuid::new_v4));
        let terminal = self
            .reserve_slot(registry, location, target.launch.is_some())
            .map_err(|fault| {
                if target.resume_token.is_some() && fault.code == "WorkspaceBusy" {
                    resume_pending()
                } else {
                    fault
                }
            })?;
        if target.new_workspace.is_some() || target.new_tab.is_some() {
            registry.publish_layout(layout, Vec::new());
        }
        if previous.is_none() {
            registry.insert_claim(location, target.keymap, client, generation);
        }
        Ok(ReservedAttach {
            location,
            generation,
            terminal,
            previous,
            resume_token,
        })
    }

    pub(crate) async fn disconnect_others(&self, active: &Active) -> Reply<u32> {
        let active = active.view();
        self.registry
            .request(move |registry| {
                let caller = active.route.client_id;
                let workspace = active.location.workspace;
                registry.require_route(active.route)?;
                registry.require_pane_writer(caller, active.location)?;
                let mut count = 0;
                for (id, client) in &mut registry.clients {
                    if *id != caller
                        && client
                            .view
                            .as_ref()
                            .is_some_and(|view| view.location.workspace == workspace)
                    {
                        count += u32::from(disconnect_client(&mut client.disconnect));
                    }
                }
                Ok(count)
            })
            .await
    }

    pub(crate) async fn release(&self, client: Uuid) {
        self.registry
            .request(move |registry| {
                registry.release_claims(client);
                registry.controllers.disconnect(client);
                if registry.clients.remove(&client).is_some() {
                    registry.publish(vec![(Group::Client(client), Vec::new())]);
                }
            })
            .await
    }

    pub(crate) async fn claim(
        &self,
        client: Uuid,
        workspace: Uuid,
        viewport: Viewport,
    ) -> Reply<u64> {
        self.registry
            .request(move |registry| {
                if registry
                    .metadata
                    .layout()
                    .locate(Some(workspace), None)
                    .is_none()
                {
                    return Err(target_gone());
                }
                if registry.clients[&client].keymap.pane_writer {
                    registry.check_workspace_writer(client, workspace)?;
                    return Ok(registry.clients[&client].generation);
                }
                if let Some(claim) = registry.claims.get(&workspace) {
                    return if claim.client == client {
                        Ok(claim.generation)
                    } else {
                        Err(busy())
                    };
                }
                if registry.has_shared_writer_or_reservation(workspace) {
                    return Err(busy());
                }
                let generation = registry.next_generation()?;
                registry
                    .claims
                    .insert(workspace, Claim { client, generation });
                registry.resize_hidden(client, viewport, None);
                registry.publish_client(client);
                Ok(generation)
            })
            .await
    }

    pub(crate) async fn move_tab(
        &self,
        active: &Active,
        tab: Uuid,
        destination: Uuid,
    ) -> Reply<TabMoved> {
        let active = active.view();
        self.registry
            .request(move |registry| {
                registry.require_publication()?;
                registry.require_route(active.route)?;

                let source_workspace = active.location.workspace;
                let client = active.route.client_id;
                registry.require_pane_writer(client, active.location)?;
                let pane_writer = registry.clients[&client].keymap.pane_writer;
                if pane_writer {
                    registry.check_workspace_writer(client, destination)?;
                } else {
                    registry.require_writer(client, destination)?;
                }

                let mut layout = registry.metadata.layout();
                let source = layout
                    .locate(Some(source_workspace), Some(tab))
                    .ok_or_else(target_gone)?;
                registry.check_pane_available(client, source)?;
                if registry.closing.iter().any(|target| {
                    target.contains(source)
                        || matches!(target, CloseTarget::Workspace(id) if *id == destination)
                }) {
                    return Err(input_busy());
                }
                let terminal = registry.movable_terminal(source)?;
                let location = Arc::make_mut(&mut layout)
                    .move_pane_tab(source, destination)
                    .map_err(|_| {
                        ProtocolFault::new(
                            "InvalidLayout",
                            "move requires a single-pane tab and a different workspace",
                        )
                    })?;

                registry
                    .slots
                    .get_mut(&location.pane)
                    .expect("validated terminal slot")
                    .workspace = destination;
                registry.terminals.insert(terminal, location);
                registry.agents.relocate_terminal(terminal, location);
                registry.publish_layout(layout, vec![terminal_projection(terminal, location)]);
                Ok(TabMoved {
                    location: Some(projection::selection(location)),
                    commit_sequence: registry.publication.sequence()?,
                })
            })
            .await
    }

    pub(crate) async fn resize_hidden(&self, active: &Active) {
        let active = active.view();
        self.registry
            .request(move |registry| {
                registry.resize_hidden(
                    active.route.client_id,
                    active.viewport.content(),
                    Some(active.location.pane),
                );
                registry.record_view(&active);
            })
            .await
    }

    pub(crate) async fn preview(&self, active: &Active, pane: Uuid) -> Reply<(Viewport, Vec<u8>)> {
        let active = active.view();
        let terminal = self
            .registry
            .request(move |registry| {
                registry.require_route(active.route)?;
                let location = Location {
                    pane,
                    ..active.location
                };
                if location == active.location || !registry.metadata.layout().contains(location) {
                    return Err(target_gone());
                }
                registry
                    .slots
                    .get(&pane)
                    .filter(|slot| slot.terminal.has_live_terminal())
                    .map(|slot| slot.terminal.clone())
                    .ok_or_else(target_gone)
            })
            .await?;
        terminal.preview().await
    }

    pub(crate) async fn switch(
        &self,
        old: &Active,
        destination: Destination,
    ) -> Reply<Option<Switched>> {
        let mut checked = None;
        let ReservedSwitch {
            location,
            terminal,
            spec,
            events,
        } = loop {
            match self
                .reserve_switch(old, destination.clone(), checked.clone())
                .await
            {
                Ok(Some(reserved)) => break reserved,
                Ok(None) => return Ok(None),
                Err(ReservationError::Fault(fault)) => return Err(fault),
                Err(ReservationError::Recovery(_)) => {
                    unreachable!("selection does not offer recovery")
                }
                Err(ReservationError::Check(paths)) => {
                    checked = Some(Arc::new(paths.validate().await?))
                }
            }
        };
        let result = transition(old, terminal, location, spec, events).await;
        let client = old.route.client_id;
        self.registry
            .request(move |registry| {
                if let Some(slot) = registry.slots.get_mut(&location.pane) {
                    slot.reserved = false;
                }
                if let Ok(switched) = &result {
                    registry.record_view(&switched.active.view());
                } else {
                    registry.remove_pane_claim(location.pane, client);
                }
                result.map(Some)
            })
            .await
    }

    async fn reserve_switch(
        &self,
        old: &Active,
        destination: Destination,
        checked: Option<Arc<CheckedCwd>>,
    ) -> Reservation<Option<ReservedSwitch>> {
        let old = old.view();
        self.transaction(move |registry, workspaces| {
            registry.require_route(old.route)?;
            registry.sweep();
            let mut layout = workspaces.metadata.layout();
            let Some((location, launch)) =
                resolve_switch(registry, &mut layout, &old, destination, checked.as_deref())?
            else {
                return Ok(None);
            };
            if location == old.location {
                return Ok(None);
            }
            let client = old.route.client_id;
            let creates_workspace = workspaces
                .metadata
                .layout()
                .locate(Some(location.workspace), None)
                .is_none();
            let pane_writer = registry.clients[&client].keymap.pane_writer;
            let lease_generation = if pane_writer {
                registry.check_pane_available(client, location)?;
                old.route.writer_lease_generation
            } else if creates_workspace {
                registry.next_generation()?
            } else {
                registry.require_writer(client, location.workspace)?
            };
            let reserved =
                workspaces.reserve_view(registry, &old, location, lease_generation, launch)?;
            if creates_workspace && !pane_writer {
                registry.claims.insert(
                    location.workspace,
                    Claim {
                        client,
                        generation: lease_generation,
                    },
                );
            }
            if reserved.spec.launch.is_some() {
                registry.publish_layout(layout, Vec::new());
            }
            Ok(Some(reserved))
        })
        .await
    }

    fn reserve_view(
        &self,
        registry: &mut Registry,
        old: &View,
        location: Location,
        lease_generation: u64,
        launch: Option<Launch>,
    ) -> Reply<ReservedSwitch> {
        let revision = old.route.view_revision.checked_add(1).ok_or_else(limit)?;
        let keymap = registry.clients[&old.route.client_id].keymap;
        registry.check_not_closing(location)?;
        let terminal = self.reserve_slot(registry, location, launch.is_some())?;
        if keymap.pane_writer {
            registry.insert_claim(location, keymap, old.route.client_id, lease_generation);
        }
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let spec = AttachSpec {
            location,
            client_id: old.route.client_id,
            generation: old.route.connection_generation,
            lease_generation,
            revision,
            dormant: true,
            keymap,
            viewport: old.viewport,
            launch,
            events,
        };
        Ok(ReservedSwitch {
            location,
            terminal,
            spec,
            events: receiver,
        })
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        let (slots, mut workers) = self
            .registry
            .request(|registry| {
                (
                    std::mem::take(&mut registry.slots),
                    std::mem::take(&mut registry.retired),
                )
            })
            .await;
        for slot in slots.into_values() {
            workers.push(slot.task);
        }
        for worker in workers {
            worker.finish().await?;
        }
        Ok(())
    }

    fn reserve_slot(
        &self,
        registry: &mut Registry,
        location: Location,
        launch: bool,
    ) -> Reply<TerminalHandle> {
        if let Some(slot) = registry.slots.get_mut(&location.pane) {
            if slot.reserved {
                return Err(busy());
            }
            slot.reserved = true;
            return Ok(slot.terminal.clone());
        }
        if !launch {
            return Err(ProtocolFault::new(
                "NoTerminal",
                "use corvid attach --new to start a terminal",
            ));
        }
        if registry.slots.len() + registry.retired.len() >= MAX_PTY_WORKERS {
            return Err(limit());
        }
        let scope = self.launch_scope(location);
        let (terminal, task) = TerminalHandle::start(scope);
        registry.slots.insert(
            location.pane,
            Slot {
                workspace: location.workspace,
                terminal: terminal.clone(),
                task,
                reserved: true,
                resume_token: None,
            },
        );
        Ok(terminal)
    }
}

fn terminal_host_ended(error: &anyhow::Error) -> bool {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if error.downcast_ref::<nix::errno::Errno>() == Some(&nix::errno::Errno::ENOENT) {
        return true;
    }
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::UnexpectedEof
        )
    })
}

impl Registry {
    fn has_live_at(&self, location: Location) -> bool {
        self.slots
            .get(&location.pane)
            .is_some_and(|slot| slot.terminal.has_live_terminal())
    }

    pub(crate) fn publish(&mut self, projections: Vec<Projection>) {
        self.commit(projections, None);
    }

    fn publish_layout(&mut self, layout: Arc<Layout>, mut projections: Vec<Projection>) {
        self.restores
            .retain(|pane, _| layout.locations().any(|location| location.pane == *pane));
        projections.push(projection::named_layout(&layout, &self.titles));
        self.commit(projections, Some(layout));
    }

    fn commit(&mut self, mut projections: Vec<Projection>, layout: Option<Arc<Layout>>) {
        for (location, key) in self.agents.resume_observations() {
            // A failed native resume must not replace the requested conversation.
            if !self
                .restores
                .get(&location.pane)
                .is_some_and(|attempt| attempt.is_starting())
            {
                self.metadata.observe_session(location, key);
            }
        }
        let layout_changed = layout.is_some();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.metadata
            .update(layout, self.agents.changed_history(self.epoch));
        #[cfg(windows)]
        self.metadata.update(layout, std::iter::empty());
        if layout_changed {
            self.metadata.publish_display(&self.titles);
        }
        projections.extend(self.agents.take_projections());
        self.publication.commit(projections);
    }

    pub(crate) fn terminal_started(&mut self, id: Uuid, location: Location) {
        self.terminals.insert(id, location);
        self.publish(vec![terminal_projection(id, location)]);
    }

    pub(crate) fn record_title(&mut self, id: Uuid, title: String) {
        let Some(location) = self.terminals.get(&id) else {
            return;
        };
        let titles = Arc::make_mut(&mut self.titles);
        if title.is_empty() {
            titles.remove(&location.pane);
        } else {
            titles.insert(location.pane, title);
        }
        self.metadata.publish_display(&self.titles);
        self.publish(vec![projection::named_layout(
            &self.metadata.layout(),
            &self.titles,
        )]);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn shell_started(&mut self, id: Uuid, root: Incarnation) {
        self.shells.add(id, root);
    }

    pub(crate) fn record_cwd(&self, terminal: Uuid, cwd: &Path, source: CwdSource) {
        let Some(&location) = self.terminals.get(&terminal) else {
            return;
        };
        let path = cwd.as_os_str().as_encoded_bytes();
        let mut layout = self.metadata.layout();
        if layout
            .last_cwd(location)
            .is_some_and(|saved| saved.path == path)
        {
            return;
        }

        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let observation = CwdObservation {
            path: path.to_vec(),
            observed_unix_ms: u64::try_from(time.as_millis()).unwrap_or(u64::MAX),
            source,
        };
        if Arc::make_mut(&mut layout)
            .record_cwd(location, observation)
            .is_ok()
        {
            self.metadata.update(Some(layout), std::iter::empty());
            self.metadata.publish_display(&self.titles);
        }
    }

    pub(crate) fn terminal_closed(&mut self, id: Uuid, association: Option<Uuid>) {
        #[cfg(windows)]
        {
            let _ = association;
            self.agents.retire_terminal(id);
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(association) = association {
            self.agents.retire_launch(association);
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(agent) = self.shells.remove(id) {
            self.agents.retire_shell(agent, ProcessLiveness::Unknown);
        }
        if let Some(location) = self.terminals.remove(&id)
            && self.titles.contains_key(&location.pane)
        {
            Arc::make_mut(&mut self.titles).remove(&location.pane);
            self.metadata.publish_display(&self.titles);
        }
        let mut projections = vec![
            (Group::Terminal(id), Vec::new()),
            projection::named_layout(&self.metadata.layout(), &self.titles),
        ];
        let mut cleared = Vec::new();
        for (client_id, client) in &mut self.clients {
            if client
                .view
                .as_ref()
                .is_some_and(|view| view.route.terminal_id == id)
            {
                if let Some(view) = &client.view {
                    self.pane_claims.remove(&view.location.pane);
                }
                client.view = None;
                cleared.push(*client_id);
            }
        }
        projections.extend(
            cleared
                .into_iter()
                .map(|client| self.client_projection(client)),
        );
        self.publish(projections);
    }

    fn record_view(&mut self, active: &View) {
        if self.set_view(active) {
            self.publish_client(active.route.client_id);
        }
    }

    fn set_view(&mut self, active: &View) -> bool {
        let route = active.route;
        let view = self
            .terminals
            .contains_key(&route.terminal_id)
            .then_some(ClientView {
                route,
                location: active.location,
                viewport: active.viewport.outer,
                input_mode: InputMode::Passthrough,
            });
        if let Some(client) = self.clients.get_mut(&route.client_id)
            && client.generation == route.connection_generation
        {
            if view.is_some()
                && let Some(slot) = self.slots.get_mut(&active.location.pane)
            {
                slot.resume_token = client.resume_token.map(|token| (route.terminal_id, token));
            }
            client.view = view;
            self.pane_claims.retain(|pane, claim| {
                claim.client != route.client_id || *pane == active.location.pane
            });
            return true;
        }
        false
    }

    pub(crate) fn record_input_mode(&mut self, route: InputRoute, mode: InputMode) {
        if let Some(view) = self
            .clients
            .get_mut(&route.client_id)
            .and_then(|client| client.view.as_mut())
            && view.route == route
            && view.input_mode != mode
        {
            view.input_mode = mode;
            self.publish_client(route.client_id);
        }
    }

    fn publish_client(&mut self, client: Uuid) {
        self.publish(vec![self.client_projection(client)]);
    }

    fn client_projection(&self, client: Uuid) -> Projection {
        let entries = self
            .client_state(client)
            .map(|state| projection::entry(EntityBody::Client(state)))
            .into_iter()
            .collect();
        (Group::Client(client), entries)
    }

    fn client_state(&self, id: Uuid) -> Option<ClientState> {
        let client = self.clients.get(&id)?;
        let view = client.view.as_ref();
        let mut writer_leases: Vec<_> = self
            .claims
            .iter()
            .filter(|(_, claim)| claim.client == id)
            .map(|(workspace, claim)| WriterLeaseState {
                workspace_id: workspace.to_string(),
                generation: claim.generation,
            })
            .collect();
        writer_leases.sort_by(|first, second| first.workspace_id.cmp(&second.workspace_id));
        let viewport = |view: &ClientView| WireViewport {
            cols: u32::from(view.viewport.cols.get()),
            rows: u32::from(view.viewport.rows.get()),
        };
        let mode = |view: &ClientView| match view.input_mode {
            InputMode::Passthrough => TuiInputMode::Passthrough as i32,
            InputMode::Command => TuiInputMode::Command as i32,
        };
        let legacy = view.filter(|_| !client.keymap.pane_writer);
        Some(ClientState {
            client_id: id.to_string(),
            connection_generation: client.generation,
            terminal_id: legacy.map(|view| view.route.terminal_id.to_string()),
            selection: legacy.map(|view| projection::selection(view.location)),
            view_revision: legacy.map(|view| view.route.view_revision),
            viewport: legacy.map(viewport),
            writer_leases,
            bindings: self.controllers.bindings(id),
            input_mode: legacy.map(mode),
            keymap: Some(ClientKeymap {
                tabs: client.keymap.tabs,
                pairing: client.keymap.pairing,
                panes: client.keymap.panes,
            }),
            pane_writer_view: view.filter(|_| client.keymap.pane_writer).map(|view| {
                PaneWriterView {
                    terminal_id: view.route.terminal_id.to_string(),
                    selection: Some(projection::selection(view.location)),
                    view_revision: view.route.view_revision,
                    viewport: Some(viewport(view)),
                    input_mode: mode(view),
                    lease_generation: view.route.writer_lease_generation,
                }
            }),
        })
    }

    fn resize_hidden(&self, client: Uuid, viewport: Viewport, visible: Option<Uuid>) {
        for (pane, slot) in &self.slots {
            if Some(*pane) != visible
                && !slot.reserved
                && self
                    .claims
                    .get(&slot.workspace)
                    .is_some_and(|claim| claim.client == client)
            {
                slot.terminal.resize_hidden(viewport);
            }
        }
    }

    fn sweep(&mut self) {
        self.retired.retain(|worker| !worker.is_finished());
        let inactive: Vec<_> = self
            .slots
            .iter()
            .filter_map(|(pane, slot)| {
                (!slot.reserved && !slot.terminal.has_live_terminal() && !slot.terminal.has_view())
                    .then_some(*pane)
            })
            .collect();
        for pane in inactive {
            if let Some(slot) = self.slots.remove(&pane) {
                self.retired.push(slot.task);
            }
        }
    }

    fn check_attach(&self, location: Location, keymap: Keymap) -> Reply<()> {
        self.check_not_closing(location)?;
        if self.claims.contains_key(&location.workspace)
            || if keymap.pane_writer {
                self.pane_claims.contains_key(&location.pane)
            } else {
                self.has_shared_writer_or_reservation(location.workspace)
            }
        {
            return Err(busy());
        }
        Ok(())
    }

    pub(crate) fn live_tab(&self, layout: &Layout, workspace: Uuid, tab: Uuid) -> Option<Location> {
        layout
            .panes(workspace, tab)
            .into_iter()
            .find(|pane| self.has_live_at(*pane))
    }

    fn require_writer(&self, client: Uuid, workspace: Uuid) -> Reply<u64> {
        let claim = self.claims.get(&workspace).ok_or_else(writer_required)?;
        if claim.client != client {
            return Err(busy());
        }
        Ok(claim.generation)
    }

    fn has_shared_writer_or_reservation(&self, workspace: Uuid) -> bool {
        self.slots.iter().any(|(pane, slot)| {
            slot.workspace == workspace && (slot.reserved || self.pane_claims.contains_key(pane))
        })
    }

    fn writer_at(&self, location: Location) -> Option<&Claim> {
        self.claims
            .get(&location.workspace)
            .or_else(|| self.pane_claims.get(&location.pane))
    }

    fn writer_view(&self, workspace: Uuid) -> Option<&ClientView> {
        self.clients
            .values()
            .filter_map(|client| client.view.as_ref())
            .filter(|view| view.location.workspace == workspace)
            .max_by_key(|view| view.route.connection_generation)
    }

    fn takeover_source(&self, location: Location) -> Reply<Option<InputRoute>> {
        self.check_not_closing(location)?;
        let Some(claim) = self.writer_at(location) else {
            return Ok(None);
        };
        self.clients
            .get(&claim.client)
            .and_then(|client| client.view.as_ref())
            .filter(|view| view.location == location)
            .map(|view| Some(view.route))
            .ok_or_else(busy)
    }

    fn resume_source(&self, location: Location, token: Option<Uuid>) -> Reply<Option<InputRoute>> {
        self.check_not_closing(location)?;
        let Some(claim) = self.writer_at(location) else {
            return Ok(None);
        };
        let token = token.ok_or_else(busy)?;
        self.clients
            .get(&claim.client)
            .filter(|client| client.resume_token == Some(token))
            .and_then(|client| client.view.as_ref())
            .filter(|view| {
                view.location == location && view.route.writer_lease_generation == claim.generation
            })
            .map(|view| Some(view.route))
            .ok_or_else(busy)
    }

    fn require_pane_writer(&self, client: Uuid, location: Location) -> Reply<u64> {
        let claim = self.writer_at(location).ok_or_else(writer_required)?;
        if claim.client != client {
            return Err(busy());
        }
        Ok(claim.generation)
    }

    fn check_workspace_writer(&self, client: Uuid, workspace: Uuid) -> Reply<()> {
        if self
            .claims
            .get(&workspace)
            .is_some_and(|claim| claim.client != client)
        {
            return Err(busy());
        }
        Ok(())
    }

    fn check_not_closing(&self, location: Location) -> Reply<()> {
        if self.closing.iter().any(|target| target.contains(location)) {
            return Err(input_busy());
        }
        Ok(())
    }

    fn check_pane_available(&self, client: Uuid, location: Location) -> Reply<()> {
        self.check_not_closing(location)?;
        if self
            .writer_at(location)
            .is_some_and(|claim| claim.client != client)
        {
            return Err(busy());
        }
        Ok(())
    }

    fn insert_claim(&mut self, location: Location, keymap: Keymap, client: Uuid, generation: u64) {
        let claim = Claim { client, generation };
        if keymap.pane_writer {
            self.pane_claims.insert(location.pane, claim);
        } else {
            self.claims.insert(location.workspace, claim);
        }
    }

    fn remove_pane_claim(&mut self, pane: Uuid, client: Uuid) {
        if self
            .pane_claims
            .get(&pane)
            .is_some_and(|claim| claim.client == client)
        {
            self.pane_claims.remove(&pane);
        }
    }

    fn release_claims(&mut self, client: Uuid) {
        self.claims.retain(|_, claim| claim.client != client);
        self.pane_claims.retain(|_, claim| claim.client != client);
    }

    fn movable_terminal(&self, location: Location) -> Reply<Uuid> {
        let slot = self.slots.get(&location.pane).ok_or_else(target_gone)?;
        if slot.reserved || slot.terminal.has_view() {
            return Err(ProtocolFault::new(
                "InputBusy",
                "select another tab before moving this terminal",
            ));
        }
        if !slot.terminal.has_live_terminal() {
            return Err(target_gone());
        }
        self.terminals
            .iter()
            .find_map(|(id, current)| (*current == location).then_some(*id))
            .ok_or_else(target_gone)
    }

    fn next_generation(&mut self) -> Reply<u64> {
        self.generation = self.generation.checked_add(1).ok_or_else(limit)?;
        Ok(self.generation)
    }
}

fn terminal_projection(id: Uuid, location: Location) -> Projection {
    let state = TerminalState {
        terminal_id: id.to_string(),
        location: Some(projection::selection(location)),
    };
    (
        Group::Terminal(id),
        vec![projection::entry(EntityBody::Terminal(state))],
    )
}

async fn open_pane(
    terminal: TerminalHandle,
    location: Location,
    spec: AttachSpec,
    receiver: mpsc::Receiver<Event>,
) -> Reply<Active> {
    let viewport = spec.viewport;
    let route = timeout(COMMAND_TIMEOUT, terminal.attach(spec))
        .await
        .map_err(|_| ProtocolFault::new("Timeout", "terminal attach timed out"))??;
    Ok(Active {
        location,
        viewport,
        route,
        terminal,
        events: receiver,
        resume_token: None,
    })
}

async fn transition(
    old: &Active,
    terminal: TerminalHandle,
    location: Location,
    spec: AttachSpec,
    events: mpsc::Receiver<Event>,
) -> Reply<Switched> {
    let active = open_pane(terminal, location, spec, events).await?;
    let paste_discarded = old.terminal.fence(old.route).await?;
    active
        .terminal
        .activate(active.route, active.viewport)
        .await?;
    Ok(Switched {
        active,
        paste_discarded,
    })
}

fn resolve_switch(
    registry: &Registry,
    layout: &mut Arc<Layout>,
    old: &View,
    destination: Destination,
    checked: Option<&CheckedCwd>,
) -> Reservation<Option<(Location, Option<Launch>)>> {
    match destination {
        Destination::Exact(location) | Destination::Open(location) => {
            if !layout.contains(location) {
                return Err(target_gone().into());
            }
            let launch =
                if matches!(destination, Destination::Open(_)) && !registry.has_live_at(location) {
                    let mut shell = new_shell();
                    shell.if_empty = true;
                    shell.cwd =
                        CwdPaths::for_launch(layout, location, &shell, None).resolved(checked)?;
                    Some(shell)
                } else {
                    None
                };
            Ok(Some((location, launch)))
        }
        Destination::Tab(TabCommand::Create) => {
            let workspace = layout
                .workspaces
                .iter()
                .find(|item| item.id == old.location.workspace)
                .ok_or_else(target_gone)?;
            let label = format!("Shell {}", workspace.tabs.len() + 1);
            let result = resolve_switch(
                registry,
                layout,
                old,
                Destination::CreateTab(label),
                checked,
            )?;
            if let Some((location, _)) = &result {
                Arc::make_mut(layout)
                    .rename_tab(*location, None)
                    .map_err(|_| target_gone())?;
            }
            Ok(result)
        }
        Destination::Tab(TabCommand::Split(axis)) => {
            resolve_switch(registry, layout, old, Destination::Split(axis), checked)
        }
        Destination::Tab(TabCommand::NextPane) => {
            let panes = layout.panes(old.location.workspace, old.location.tab);
            let Some(index) = panes.iter().position(|pane| *pane == old.location) else {
                return Err(target_gone().into());
            };
            let client = old.route.client_id;
            Ok((1..panes.len())
                .map(|offset| panes[(index + offset) % panes.len()])
                .find(|pane| {
                    registry.has_live_at(*pane)
                        && registry.check_pane_available(client, *pane).is_ok()
                })
                .map(|pane| (pane, None)))
        }
        Destination::Tab(command) => {
            Ok(select_tab(registry, layout, old, command).map(|location| (location, None)))
        }
        creation => {
            let mut launch = new_shell();
            let inherited = saved_directory(layout, old.location);
            let layout = Arc::make_mut(layout);
            let location = match creation {
                Destination::CreateWorkspace(label, directory) => {
                    layout.add_workspace_in(label, "Shell".into(), directory)
                }
                Destination::CreateTab(label) => layout.add_tab(old.location.workspace, label),
                Destination::Split(axis) => layout.split_pane(old.location, axis),
                _ => unreachable!("all other destinations have already been handled"),
            }
            .map_err(|_| invalid_layout())?;
            let fallback = inherited.map(CwdFallback::Inherited);
            launch.cwd = CwdPaths::defaults(layout, location, None, fallback).resolved(checked)?;
            Ok(Some((location, Some(launch))))
        }
    }
}

fn new_shell() -> Launch {
    Launch {
        argv: Vec::new(),
        cwd: None,
        provider: None,
        if_empty: false,
    }
}

fn select_tab(
    registry: &Registry,
    layout: &Layout,
    active: &View,
    command: TabCommand,
) -> Option<Location> {
    let old = active.location;
    let client = active.route.client_id;
    let select = |tab| {
        layout.panes(old.workspace, tab).into_iter().find(|pane| {
            registry.has_live_at(*pane) && registry.check_pane_available(client, *pane).is_ok()
        })
    };
    let workspace = layout
        .workspaces
        .iter()
        .find(|item| item.id == old.workspace)?;
    match command {
        TabCommand::Index(index) => {
            let tab = workspace.tabs.get(index)?.id;
            return select(tab).or_else(|| registry.live_tab(layout, old.workspace, tab));
        }
        TabCommand::Select(tab) => {
            return select(tab).or_else(|| registry.live_tab(layout, old.workspace, tab));
        }
        TabCommand::Next | TabCommand::Previous => {}
        TabCommand::Create | TabCommand::Split(_) | TabCommand::NextPane => return None,
    }

    let index = workspace.tabs.iter().position(|tab| tab.id == old.tab)?;
    let len = workspace.tabs.len();
    (1..len).find_map(|offset| {
        let index = if matches!(command, TabCommand::Next) {
            (index + offset) % len
        } else {
            (index + len - offset) % len
        };
        select(workspace.tabs[index].id)
    })
}

fn resolve(
    registry: &Registry,
    layout: &mut Arc<Layout>,
    target: &AttachTarget,
    prefer_available: bool,
) -> Reply<Location> {
    if (target.new_workspace.is_some() && (target.workspace.is_some() || target.tab.is_some()))
        || (target.new_tab.is_some() && target.tab.is_some())
        || (target.pane.is_some() && (target.tab.is_none() || target.workspace.is_none()))
        || ((target.new_workspace.is_some() || target.new_tab.is_some()) && target.launch.is_none())
        || (target.workspace_directory.is_some() && target.new_workspace.is_none())
    {
        return Err(ProtocolFault::new(
            "InvalidMessage",
            "conflicting workspace or tab selection",
        ));
    }
    if let Some(label) = &target.new_workspace {
        let manual = target
            .new_tab
            .as_ref()
            .filter(|name| !name.is_empty())
            .cloned();
        let tab = manual.clone().unwrap_or_else(|| "Shell".to_owned());
        let location = Arc::make_mut(layout)
            .add_workspace_in(label.clone(), tab, target.workspace_directory.clone())
            .map_err(|_| invalid_layout())?;
        Arc::make_mut(layout)
            .rename_tab(location, manual)
            .map_err(|_| invalid_layout())?;
        return Ok(location);
    }
    let location = layout
        .locate(target.workspace, target.tab)
        .ok_or_else(target_gone)?;
    if let Some(pane) = target.pane {
        let exact = Location { pane, ..location };
        return layout
            .contains(exact)
            .then_some(exact)
            .ok_or_else(target_gone);
    }
    if let Some(label) = &target.new_tab {
        let automatic = label.is_empty();
        let label = if automatic {
            "Shell".to_owned()
        } else {
            label.clone()
        };
        let location = Arc::make_mut(layout)
            .add_tab(location.workspace, label)
            .map_err(|_| invalid_layout())?;
        if automatic {
            Arc::make_mut(layout)
                .rename_tab(location, None)
                .map_err(|_| invalid_layout())?;
        }
        return Ok(location);
    }
    if target.launch.as_ref().is_none_or(|launch| launch.if_empty) {
        let workspace = layout
            .workspaces
            .iter()
            .find(|item| item.id == location.workspace);
        let mut live = workspace
            .into_iter()
            .flat_map(|item| &item.tabs)
            .filter(|tab| target.tab.is_none_or(|id| id == tab.id))
            .flat_map(|tab| layout.panes(location.workspace, tab.id))
            .filter(|location| registry.has_live_at(*location));
        if let Some(first) = live.next() {
            if prefer_available && registry.check_attach(first, target.keymap).is_err() {
                return Ok(live
                    .find(|location| registry.check_attach(*location, target.keymap).is_ok())
                    .unwrap_or(first));
            }
            return Ok(first);
        }
    }
    Ok(location)
}

fn saved_directory(layout: &Layout, location: Location) -> Option<PathBuf> {
    layout
        .last_cwd(location)
        .and_then(|cwd| crate::platform::decode_os_string(&cwd.path))
        .map(PathBuf::from)
}

type Reservation<T> = Result<T, ReservationError>;

enum ReservationError {
    Fault(ProtocolFault),
    Recovery(Box<corvid_protocol::wire::PaneRestoreResult>),
    Check(CwdPaths),
}

impl From<ProtocolFault> for ReservationError {
    fn from(fault: ProtocolFault) -> Self {
        Self::Fault(fault)
    }
}

#[derive(PartialEq, Eq)]
enum CwdFallback {
    Provided(PathBuf),
    Inherited(PathBuf),
}

#[derive(PartialEq, Eq)]
enum CwdPaths {
    Explicit(PathBuf),
    Defaults {
        preferred: Option<PathBuf>,
        workspace: Option<Vec<u8>>,
        fallback: Option<CwdFallback>,
    },
}

struct CheckedCwd {
    paths: CwdPaths,
    result: Reply<Option<PathBuf>>,
}

impl CwdPaths {
    fn for_launch(
        layout: &Layout,
        location: Location,
        launch: &Launch,
        fallback: Option<PathBuf>,
    ) -> Self {
        if let Some(path) = &launch.cwd {
            return Self::Explicit(path.clone());
        }
        let preferred = fallback
            .is_none()
            .then(|| saved_directory(layout, location))
            .flatten();
        Self::defaults(
            layout,
            location,
            preferred,
            fallback.map(CwdFallback::Provided),
        )
    }

    fn defaults(
        layout: &Layout,
        location: Location,
        preferred: Option<PathBuf>,
        fallback: Option<CwdFallback>,
    ) -> Self {
        Self::Defaults {
            preferred,
            workspace: layout
                .workspace_directory(location.workspace)
                .map(<[u8]>::to_vec),
            fallback,
        }
    }

    fn resolved(self, checked: Option<&CheckedCwd>) -> Reservation<Option<PathBuf>> {
        // Re-resolve under the registry after checking the filesystem. A concurrent
        // layout edit can change the selected paths while the check is in flight.
        if let Some(checked) = checked.filter(|checked| checked.paths == self) {
            return Ok(checked.result.clone()?);
        }
        if matches!(
            &self,
            Self::Defaults {
                preferred: None,
                workspace: None,
                fallback: None
            }
        ) {
            return Ok(None);
        }
        Err(ReservationError::Check(self))
    }

    async fn validate(self) -> Reply<CheckedCwd> {
        blocking(move || {
            let result = self.check();
            CheckedCwd {
                paths: self,
                result,
            }
        })
        .await
        .map_err(|_| {
            ProtocolFault::new(
                "DirectoryUnavailable",
                "could not check the launch directory",
            )
        })
    }

    fn check(&self) -> Reply<Option<PathBuf>> {
        match self {
            Self::Explicit(path) => {
                if !path.is_dir() {
                    return Err(ProtocolFault::new(
                        "InvalidMessage",
                        "launch directory does not exist",
                    ));
                }
                Ok(Some(path.clone()))
            }
            Self::Defaults {
                preferred,
                workspace,
                fallback,
            } => {
                let directory = if let Some(path) = preferred.as_ref().filter(|path| path.is_dir())
                {
                    Some(path.clone())
                } else if let Some(path) = workspace {
                    let path = crate::platform::decode_os_string(path)
                        .map(PathBuf::from)
                        .ok_or_else(|| {
                            ProtocolFault::new(
                                "InvalidDirectory",
                                "The saved workspace folder is invalid.",
                            )
                        })?;
                    Some(path)
                } else {
                    match fallback {
                        Some(CwdFallback::Provided(path)) => Some(path.clone()),
                        Some(CwdFallback::Inherited(path)) if path.is_dir() => Some(path.clone()),
                        _ => None,
                    }
                };
                if let Some(path) = &directory {
                    crate::directories::validate(path)?;
                }
                Ok(directory)
            }
        }
    }
}

fn target_gone() -> ProtocolFault {
    ProtocolFault::new("TargetGone", "workspace or tab does not exist")
}

fn disconnect_client(signal: &mut Option<oneshot::Sender<()>>) -> bool {
    signal.take().is_some_and(|signal| signal.send(()).is_ok())
}

fn busy() -> ProtocolFault {
    ProtocolFault::new("WorkspaceBusy", "pane or workspace already has a writer")
}

fn resume_pending() -> ProtocolFault {
    ProtocolFault::new(
        "WriterResumePending",
        "waiting for the terminal input route to settle",
    )
}

fn input_busy() -> ProtocolFault {
    ProtocolFault::new("InputBusy", "the layout is changing")
}

fn writer_required() -> ProtocolFault {
    ProtocolFault::new(
        "WriterRequired",
        "claim the destination workspace before switching",
    )
}

fn limit() -> ProtocolFault {
    ProtocolFault::new("LimitExceeded", "terminal worker limit reached")
}

fn invalid_layout() -> ProtocolFault {
    ProtocolFault::new(
        "InvalidMessage",
        "invalid workspace label, tab label, or layout size",
    )
}

#[cfg(test)]
mod directory_tests {
    use super::{CwdFallback, CwdPaths};

    #[test]
    fn workspace_directory_takes_priority_over_the_source_pane_directory() {
        let workspace = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let paths = CwdPaths::Defaults {
            preferred: None,
            workspace: Some(workspace.path().as_os_str().as_encoded_bytes().to_vec()),
            fallback: Some(CwdFallback::Inherited(source.path().to_owned())),
        };
        assert_eq!(paths.check().unwrap().as_deref(), Some(workspace.path()));
    }

    #[test]
    fn a_missing_inherited_directory_leaves_the_shell_default() {
        let workspace = tempfile::tempdir().unwrap();
        let paths = CwdPaths::Defaults {
            preferred: None,
            workspace: None,
            fallback: Some(CwdFallback::Inherited(workspace.path().join("missing"))),
        };
        assert_eq!(paths.check().unwrap(), None);
    }
}
