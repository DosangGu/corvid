mod close;
mod controllers;
mod focus;
#[cfg(target_os = "linux")]
mod shell;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
#[cfg(target_os = "linux")]
use corvid_core::agent::ProcessLiveness;
#[cfg(target_os = "linux")]
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
use tokio::task::JoinHandle;
use tokio::time::timeout;
use uuid::Uuid;

use crate::agents::Agents;
#[cfg(target_os = "linux")]
use crate::agents::{Hook, HookBinding};
use crate::collector::{CollectorDiagnostics, HookEnvironment};
use crate::metadata::MetadataUpdates;
#[cfg(target_os = "linux")]
use crate::process::{self, HookProducer, Incarnation, ProbeBudget, ProcessImage};
use crate::projection;
use crate::publication::{Capture, Group, Projection, Publication};
use crate::terminal::{
    AttachSpec, Event, HostConfig, InputMode, Keymap, Launch, LaunchScope, RetainedTerminal,
    TabCommand, TerminalHandle, TuiViewport,
};

pub(crate) use close::Closed;
pub(crate) use controllers::ControllerCommand;
use controllers::Controllers;
pub(crate) use focus::{FocusDispatch, FocusRecovery};
#[cfg(target_os = "linux")]
use shell::Shells;

pub(crate) const MAX_PTY_WORKERS: usize = 32;
const EVENT_CAPACITY: usize = 4;

type Reply<T> = Result<T, ProtocolFault>;

pub(crate) struct AttachTarget {
    pub(crate) workspace: Option<Uuid>,
    pub(crate) tab: Option<Uuid>,
    pub(crate) pane: Option<Uuid>,
    pub(crate) new_workspace: Option<String>,
    pub(crate) new_tab: Option<String>,
    pub(crate) keymap: Keymap,
    pub(crate) viewport: TuiViewport,
    pub(crate) launch: Option<Launch>,
    pub(crate) take_over: bool,
    pub(crate) expected_terminal: Option<Uuid>,
}

pub(crate) struct Active {
    pub(crate) location: Location,
    pub(crate) viewport: TuiViewport,
    pub(crate) route: InputRoute,
    pub(crate) terminal: TerminalHandle,
    pub(crate) events: mpsc::Receiver<Event>,
}

pub(crate) struct Observation {
    pub(crate) location: Location,
    pub(crate) terminal_id: Uuid,
    pub(crate) terminal: TerminalHandle,
    pub(crate) generation: u64,
    pub(crate) promoted: oneshot::Receiver<Active>,
}

struct Observer {
    location: Location,
    terminal: Uuid,
    generation: u64,
    viewport: TuiViewport,
    keymap: Keymap,
    focuses: mpsc::Sender<FocusDispatch>,
    promote: Option<oneshot::Sender<Active>>,
    disconnect: Option<oneshot::Sender<()>>,
}

pub(crate) enum Destination {
    Exact(Location),
    Open(Location),
    CreateWorkspace(String),
    CreateTab(String),
    Split(Axis),
    Tab(TabCommand),
}

pub(crate) struct Switched {
    pub(crate) active: Active,
    pub(crate) paste_discarded: bool,
}

struct ReservedSwitch {
    location: Location,
    terminal: TerminalHandle,
    spec: AttachSpec,
    events: mpsc::Receiver<Event>,
}

pub(crate) struct Workspaces {
    pub(crate) collector: CollectorDiagnostics,
    registry: Arc<Mutex<Registry>>,
    metadata: MetadataUpdates,
    #[cfg(target_os = "linux")]
    boot_id: Option<Uuid>,
    #[cfg(target_os = "linux")]
    probes: Arc<ProbeBudget>,
    hook_environment: Arc<HookEnvironment>,
    host: Option<HostConfig>,
}

pub(crate) struct Registry {
    pub(crate) agents: Agents,
    metadata: MetadataUpdates,
    #[cfg(target_os = "linux")]
    epoch: Uuid,
    publication: Publication,
    terminals: HashMap<Uuid, Location>,
    #[cfg(target_os = "linux")]
    shells: Shells,
    clients: HashMap<Uuid, Client>,
    observers: HashMap<Uuid, Observer>,
    controllers: Controllers,
    generation: u64,
    claims: HashMap<Uuid, Claim>,
    pane_claims: HashMap<Uuid, Claim>,
    closing: Vec<CloseTarget>,
    slots: HashMap<Uuid, Slot>,
    retired: Vec<JoinHandle<()>>,
}

struct Client {
    generation: u64,
    keymap: Keymap,
    view: Option<ClientView>,
    focuses: mpsc::Sender<FocusDispatch>,
    disconnect: Option<oneshot::Sender<()>>,
    #[cfg(target_os = "linux")]
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
    task: JoinHandle<()>,
    reserved: bool,
}

impl Workspaces {
    pub(crate) fn new(metadata: MetadataUpdates, hook_environment: Arc<HookEnvironment>) -> Self {
        let publication = Publication::seed(projection::layout(&metadata.layout()));
        #[cfg(target_os = "linux")]
        let epoch = Uuid::parse_str(&hook_environment.identity.epoch).expect("valid daemon epoch");
        Self {
            collector: CollectorDiagnostics::default(),
            registry: Arc::new(Mutex::new(Registry {
                agents: Agents::default(),
                metadata: metadata.clone(),
                #[cfg(target_os = "linux")]
                epoch,
                publication,
                terminals: HashMap::new(),
                #[cfg(target_os = "linux")]
                shells: Shells::default(),
                clients: HashMap::new(),
                observers: HashMap::new(),
                controllers: Controllers::default(),
                generation: 0,
                claims: HashMap::new(),
                pane_claims: HashMap::new(),
                closing: Vec::new(),
                slots: HashMap::new(),
                retired: Vec::new(),
            })),
            metadata,
            #[cfg(target_os = "linux")]
            boot_id: process::boot_id(),
            #[cfg(target_os = "linux")]
            probes: Arc::new(ProbeBudget::default()),
            hook_environment,
            host: None,
        }
    }

    pub(crate) fn enable_terminal_hosts(mut self, runtime: &Path) -> Result<Self> {
        self.host = Some(HostConfig::new(runtime)?);
        Ok(self)
    }

    pub(crate) fn is_idle(&self) -> bool {
        let mut registry = self.lock_registry();
        registry.sweep();
        registry.clients.is_empty()
            && registry.claims.is_empty()
            && registry.pane_claims.is_empty()
            && registry.slots.is_empty()
            && registry.retired.is_empty()
    }

    pub(crate) async fn retain_terminals(
        &self,
        retain: bool,
    ) -> Result<Vec<(Location, RetainedTerminal)>> {
        let terminals: Vec<_> = {
            let registry = self.lock_registry();
            registry
                .slots
                .iter()
                .filter_map(|(pane, slot)| {
                    registry.terminals.iter().find_map(|(_, location)| {
                        (location.pane == *pane).then(|| (*location, slot.terminal.clone()))
                    })
                })
                .collect()
        };
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
            self.lock_registry().slots.insert(
                location.pane,
                Slot {
                    workspace: location.workspace,
                    terminal,
                    task,
                    reserved: false,
                },
            );
        }
        Ok(())
    }

    fn launch_scope(&self, location: Location) -> LaunchScope {
        LaunchScope {
            location,
            metadata: self.metadata.clone(),
            registry: Arc::downgrade(&self.registry),
            #[cfg(target_os = "linux")]
            boot_id: self.boot_id,
            #[cfg(target_os = "linux")]
            probes: Arc::clone(&self.probes),
            hook_environment: Arc::clone(&self.hook_environment),
            host: self.host.clone(),
        }
    }

    fn lock_registry(&self) -> MutexGuard<'_, Registry> {
        self.registry
            .lock()
            .expect("workspace registry lock poisoned")
    }

    pub(crate) fn agents(&self) -> AgentList {
        self.lock_registry().agents.inventory()
    }

    #[cfg(windows)]
    pub(crate) async fn tick_native_agents(&self) {
        let roots = self.lock_registry().agents.roots();
        let found = if roots.is_empty() {
            Vec::new()
        } else {
            tokio::task::spawn_blocking(move || crate::native_process::scan(&roots))
                .await
                .unwrap_or_default()
        };
        let mut registry = self.lock_registry();
        let Registry {
            agents, terminals, ..
        } = &mut *registry;
        if agents.sample_shells(found, terminals) {
            registry.publish(Vec::new());
        }
    }

    #[cfg(windows)]
    pub(crate) async fn apply_native_hook(
        &self,
        hook: crate::hook_data::Hook,
        pid: u32,
    ) -> Reply<()> {
        let unauthorized =
            || ProtocolFault::new("Unauthorized", "hook has no verified Windows agent owner");
        let root = self
            .lock_registry()
            .agents
            .hook_root(hook.association)
            .ok_or_else(unauthorized)?;
        let provider = hook.provider;
        let owner = tokio::task::spawn_blocking(move || {
            crate::native_process::verify_hook(pid, root, provider)
        })
        .await
        .ok()
        .flatten()
        .ok_or_else(unauthorized)?;
        let mut registry = self.lock_registry();
        let location = *registry
            .terminals
            .get(&hook.association)
            .ok_or_else(unauthorized)?;
        let result = registry.agents.apply_hook(hook, owner, location);
        registry.publish(Vec::new());
        result
    }

    pub(crate) fn clients(&self) -> ClientList {
        let registry = self.lock_registry();
        let mut clients: Vec<_> = registry
            .clients
            .keys()
            .filter_map(|id| registry.client_state(*id))
            .collect();
        clients.sort_by(|first, second| first.client_id.cmp(&second.client_id));
        ClientList { clients }
    }

    pub(crate) fn snapshot(&self, watch: bool) -> Reply<Capture> {
        self.lock_registry().publication.capture(watch)
    }

    pub(crate) fn public_sequence(&self) -> Reply<u64> {
        self.lock_registry().publication.sequence()
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn tick_agents(&self) {
        self.poll_shell_agents();
        let mut registry = self.lock_registry();
        registry.agents.tick();
        registry.publish(Vec::new());
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn hook_binding(&self, association: Uuid) -> HookBinding {
        self.lock_registry().agents.binding(association)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn apply_hook(
        &self,
        hook: Hook,
        owner: ProcessImage,
        producer: HookProducer,
    ) -> Reply<()> {
        let mut registry = self.lock_registry();
        let notice = registry.agents.apply_hook(hook, owner, producer)?;
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
    }

    pub(crate) fn runtime_status(&self) -> (bool, CollectorStatus) {
        let registry = self.lock_registry();
        let terminal_live = registry
            .slots
            .values()
            .any(|slot| slot.terminal.has_live_terminal());
        #[cfg(target_os = "linux")]
        let mut collector = registry.agents.collector_status();
        #[cfg(windows)]
        let mut collector = CollectorStatus::default();
        collector.traffic = Some(self.collector.traffic());
        (terminal_live, collector)
    }

    pub(crate) async fn attach(
        &self,
        client: Uuid,
        mut target: AttachTarget,
        focuses: mpsc::Sender<FocusDispatch>,
        disconnect: Option<oneshot::Sender<()>>,
        #[cfg(target_os = "linux")] notices: Option<mpsc::Sender<Provider>>,
    ) -> Reply<Active> {
        let (location, generation, terminal, previous) = self.reserve(client, &mut target)?;
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
                    },
                    Some(events),
                )
            })
        } else {
            open_pane(terminal, location, spec, receiver)
                .await
                .map(|active| (active, None))
        };

        let mut registry = self.lock_registry();
        if let Some(slot) = registry.slots.get_mut(&location.pane) {
            slot.reserved = false;
        }
        match opened {
            Ok((active, _previous_events)) => {
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
                        view: None,
                        focuses,
                        disconnect,
                        #[cfg(target_os = "linux")]
                        notices,
                    },
                );
                if let Some(previous) = previous {
                    registry.set_view(&active);
                    let projections = vec![
                        registry.client_projection(previous.client_id),
                        registry.client_projection(client),
                    ];
                    registry.publish(projections);
                } else {
                    registry.record_view(&active);
                }
                Ok(active)
            }
            Err(fault) => {
                registry.release_claims(client);
                Err(fault)
            }
        }
    }

    pub(crate) fn observe(
        &self,
        client: Uuid,
        target: &AttachTarget,
        focuses: mpsc::Sender<FocusDispatch>,
        disconnect: Option<oneshot::Sender<()>>,
    ) -> Reply<Observation> {
        if target.launch.is_some()
            || target.new_workspace.is_some()
            || target.new_tab.is_some()
            || target.take_over
        {
            return Err(ProtocolFault::new(
                "InvalidMessage",
                "an observer requires an existing terminal",
            ));
        }
        let mut registry = self.lock_registry();
        let mut layout = self.metadata.layout();
        let mut location = resolve(&registry, &mut layout, target, false)?;
        if target.tab.is_none()
            && target.pane.is_none()
            && let Some(view) = registry.writer_view(location.workspace)
        {
            location = view.location;
        }
        let terminal = registry
            .slots
            .get(&location.pane)
            .filter(|slot| !slot.reserved && slot.terminal.has_live_terminal())
            .map(|slot| slot.terminal.clone())
            .ok_or_else(target_gone)?;
        let id = registry
            .terminals
            .iter()
            .find_map(|(&id, &current)| (current == location).then_some(id))
            .ok_or_else(target_gone)?;
        let generation = registry.next_generation()?;
        let (promote, promoted) = oneshot::channel();
        registry.observers.insert(
            client,
            Observer {
                location,
                terminal: id,
                generation,
                viewport: target.viewport,
                keymap: target.keymap,
                focuses,
                promote: Some(promote),
                disconnect,
            },
        );
        Ok(Observation {
            location,
            terminal_id: id,
            terminal,
            generation,
            promoted,
        })
    }

    pub(crate) fn resize_observer(&self, client: Uuid, viewport: Viewport) {
        if let Some(observer) = self.lock_registry().observers.get_mut(&client) {
            observer.viewport.outer = viewport;
        }
    }

    pub(crate) fn can_observe(&self, location: Location, terminal: Uuid) -> bool {
        let registry = self.lock_registry();
        registry.terminals.get(&terminal) == Some(&location) && registry.has_live_at(location)
    }

    pub(crate) async fn handoff(&self, old: &Active) -> Reply<()> {
        let (spec, receiver, promote, focuses) = {
            let mut registry = self.lock_registry();
            registry.require_route(old.route)?;
            registry.require_pane_writer(old.route.client_id, old.location)?;
            let shared = registry.clients[&old.route.client_id].keymap.pane_writer;
            let mut eligible = registry.observers.iter().filter(|(_, observer)| {
                observer.location == old.location
                    && observer.terminal == old.route.terminal_id
                    && observer.keymap.handoff
                    && (!shared || observer.keymap.pane_writer)
                    && observer
                        .promote
                        .as_ref()
                        .is_some_and(|send| !send.is_closed())
            });
            let observer_required = || {
                ProtocolFault::new(
                    "ObserverRequired",
                    "handoff requires one eligible observer on this pane",
                )
            };
            let (&client, _) = eligible.next().ok_or_else(observer_required)?;
            if eligible.next().is_some() {
                return Err(observer_required());
            }
            let lease_generation = registry.next_generation()?;
            let observer = registry
                .observers
                .get_mut(&client)
                .expect("selected observer");
            let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
            let spec = AttachSpec {
                location: observer.location,
                client_id: client,
                generation: observer.generation,
                lease_generation,
                revision: 2,
                dormant: false,
                keymap: observer.keymap,
                viewport: observer.viewport,
                launch: None,
                events,
            };
            (
                spec,
                receiver,
                observer.promote.take().expect("eligible observer"),
                observer.focuses.clone(),
            )
        };
        let client = spec.client_id;
        let viewport = spec.viewport;
        let keymap = spec.keymap;
        let (route, _previous_events) = match old.terminal.transfer(old.route, spec).await {
            Ok(transfer) => transfer,
            Err(error) => {
                if let Some(observer) = self.lock_registry().observers.get_mut(&client) {
                    observer.promote = Some(promote);
                }
                return Err(error);
            }
        };
        let active = Active {
            route,
            viewport,
            location: old.location,
            terminal: old.terminal.clone(),
            events: receiver,
        };
        {
            let mut registry = self.lock_registry();
            let disconnect = registry
                .observers
                .remove(&client)
                .and_then(|observer| observer.disconnect);
            if let Some(previous) = registry.clients.get_mut(&old.route.client_id) {
                previous.view = None;
            }
            registry.pane_claims.remove(&old.location.pane);
            registry.claims.remove(&old.location.workspace);
            registry.insert_claim(
                active.location,
                keymap,
                client,
                route.writer_lease_generation,
            );
            registry.clients.insert(
                client,
                Client {
                    generation: route.connection_generation,
                    keymap,
                    view: None,
                    focuses,
                    disconnect,
                    #[cfg(target_os = "linux")]
                    notices: None,
                },
            );
            registry.set_view(&active);
            registry.resize_hidden(client, viewport.content(), Some(active.location.pane));
            let projections = vec![
                registry.client_projection(old.route.client_id),
                registry.client_projection(client),
            ];
            registry.publish(projections);
        }
        if promote.send(active).is_err() {
            self.release(client);
        }
        Ok(())
    }

    fn reserve(
        &self,
        client: Uuid,
        target: &mut AttachTarget,
    ) -> Reply<(Location, u64, TerminalHandle, Option<InputRoute>)> {
        let mut registry = self.lock_registry();
        registry.sweep();
        let mut layout = self.metadata.layout();
        let mut location = resolve(
            &registry,
            &mut layout,
            target,
            target.keymap.pane_writer && !target.take_over,
        )?;
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
            return Err(ProtocolFault::new(
                "TargetGone",
                "the observed terminal has changed",
            ));
        }
        let previous = if target.take_over {
            registry.takeover_source(location)?
        } else {
            None
        };
        if previous.is_none() {
            registry.check_attach(location, target.keymap)?;
        }
        if let Some(launch) = &mut target.launch
            && (!launch.if_empty || !registry.has_live_at(location))
        {
            prepare_cwd(&layout, location, Some(launch))?;
        }
        let generation = registry.next_generation()?;
        let terminal = self.reserve_slot(&mut registry, location, target.launch.is_some())?;
        if target.new_workspace.is_some() || target.new_tab.is_some() {
            registry.publish_layout(layout, Vec::new());
        }
        if previous.is_none() {
            registry.insert_claim(location, target.keymap, client, generation);
        }
        Ok((location, generation, terminal, previous))
    }

    pub(crate) fn disconnect_others(&self, active: &Active) -> Reply<u32> {
        let mut registry = self.lock_registry();
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
        for observer in registry.observers.values_mut() {
            if observer.location.workspace == workspace {
                count += u32::from(disconnect_client(&mut observer.disconnect));
            }
        }
        Ok(count)
    }

    pub(crate) fn release(&self, client: Uuid) {
        let mut registry = self.lock_registry();
        registry.observers.remove(&client);
        registry.release_claims(client);
        registry.clients.remove(&client);
        registry.controllers.disconnect(client);
        registry.publish(vec![(Group::Client(client), Vec::new())]);
    }

    pub(crate) fn claim(&self, client: Uuid, workspace: Uuid, viewport: Viewport) -> Reply<u64> {
        let mut registry = self.lock_registry();
        if self
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
    }

    pub(crate) fn move_tab(
        &self,
        active: &Active,
        tab: Uuid,
        destination: Uuid,
    ) -> Reply<TabMoved> {
        let mut registry = self.lock_registry();
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

        let mut layout = self.metadata.layout();
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
    }

    pub(crate) fn resize_hidden(&self, active: &Active) {
        let mut registry = self.lock_registry();
        registry.resize_hidden(
            active.route.client_id,
            active.viewport.content(),
            Some(active.location.pane),
        );
        registry.record_view(active);
    }

    pub(crate) async fn preview(&self, active: &Active, pane: Uuid) -> Reply<(Viewport, Vec<u8>)> {
        let terminal = {
            let registry = self.lock_registry();
            registry.require_route(active.route)?;
            let location = Location {
                pane,
                ..active.location
            };
            if location == active.location || !self.metadata.layout().contains(location) {
                return Err(target_gone());
            }
            registry
                .slots
                .get(&pane)
                .filter(|slot| slot.terminal.has_live_terminal())
                .map(|slot| slot.terminal.clone())
                .ok_or_else(target_gone)?
        };
        terminal.preview().await
    }

    pub(crate) async fn switch(
        &self,
        old: &Active,
        destination: Destination,
    ) -> Reply<Option<Switched>> {
        let Some(ReservedSwitch {
            location,
            terminal,
            spec,
            events,
        }) = self.reserve_switch(old, destination)?
        else {
            return Ok(None);
        };
        let result = transition(old, terminal, location, spec, events).await;
        let mut registry = self.lock_registry();
        if let Some(slot) = registry.slots.get_mut(&location.pane) {
            slot.reserved = false;
        }
        if let Ok(switched) = &result {
            registry.record_view(&switched.active);
        } else {
            registry.remove_pane_claim(location.pane, old.route.client_id);
        }
        result.map(Some)
    }

    fn reserve_switch(
        &self,
        old: &Active,
        destination: Destination,
    ) -> Reply<Option<ReservedSwitch>> {
        let mut registry = self.lock_registry();
        registry.require_route(old.route)?;
        registry.sweep();
        let mut layout = self.metadata.layout();
        let Some((location, launch)) = resolve_switch(&registry, &mut layout, old, destination)?
        else {
            return Ok(None);
        };
        if location == old.location {
            return Ok(None);
        }
        let client = old.route.client_id;
        let creates_workspace = self
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
        let reserved = self.reserve_view(&mut registry, old, location, lease_generation, launch)?;
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
    }

    fn reserve_view(
        &self,
        registry: &mut Registry,
        old: &Active,
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
        let (slots, mut workers) = {
            let mut registry = self.lock_registry();
            (
                std::mem::take(&mut registry.slots),
                std::mem::take(&mut registry.retired),
            )
        };
        for slot in slots.into_values() {
            workers.push(slot.task);
        }
        for worker in workers {
            worker.await?;
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
            },
        );
        Ok(terminal)
    }
}

fn terminal_host_ended(error: &anyhow::Error) -> bool {
    #[cfg(target_os = "linux")]
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
        projections.push(projection::layout(&layout));
        self.commit(projections, Some(layout));
    }

    fn commit(&mut self, mut projections: Vec<Projection>, layout: Option<Arc<Layout>>) {
        #[cfg(target_os = "linux")]
        self.metadata
            .update(layout, self.agents.changed_history(self.epoch));
        #[cfg(windows)]
        self.metadata.update(layout, std::iter::empty());
        projections.extend(self.agents.take_projections());
        self.publication.commit(projections);
    }

    pub(crate) fn terminal_started(&mut self, id: Uuid, location: Location) {
        self.terminals.insert(id, location);
        self.publish(vec![terminal_projection(id, location)]);
    }

    #[cfg(target_os = "linux")]
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
        }
    }

    pub(crate) fn terminal_closed(&mut self, id: Uuid, association: Option<Uuid>) {
        #[cfg(windows)]
        {
            let _ = association;
            self.agents.retire_terminal(id);
        }
        #[cfg(target_os = "linux")]
        if let Some(association) = association {
            self.agents.retire_launch(association);
        }
        #[cfg(target_os = "linux")]
        if let Some(agent) = self.shells.remove(id) {
            self.agents.retire_shell(agent, ProcessLiveness::Unknown);
        }
        self.terminals.remove(&id);
        let mut projections = vec![(Group::Terminal(id), Vec::new())];
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

    fn record_view(&mut self, active: &Active) {
        if self.set_view(active) {
            self.publish_client(active.route.client_id);
        }
    }

    fn set_view(&mut self, active: &Active) -> bool {
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
    old: &Active,
    destination: Destination,
) -> Reply<Option<(Location, Option<Launch>)>> {
    match destination {
        Destination::Exact(location) | Destination::Open(location) => {
            if !layout.contains(location) {
                return Err(target_gone());
            }
            let launch = if matches!(destination, Destination::Open(_)) {
                let mut shell = new_shell();
                shell.if_empty = true;
                prepare_cwd(layout, location, Some(&mut shell))?;
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
            resolve_switch(registry, layout, old, Destination::CreateTab(label))
        }
        Destination::Tab(TabCommand::Split(axis)) => {
            resolve_switch(registry, layout, old, Destination::Split(axis))
        }
        Destination::Tab(TabCommand::NextPane) => {
            let panes = layout.panes(old.location.workspace, old.location.tab);
            let Some(index) = panes.iter().position(|pane| *pane == old.location) else {
                return Err(target_gone());
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
            prepare_cwd(layout, old.location, Some(&mut launch))?;
            let layout = Arc::make_mut(layout);
            let location = match creation {
                Destination::CreateWorkspace(label) => layout.add_workspace(label, "Shell".into()),
                Destination::CreateTab(label) => layout.add_tab(old.location.workspace, label),
                Destination::Split(axis) => layout.split_pane(old.location, axis),
                _ => unreachable!("all other destinations have already been handled"),
            }
            .map_err(|_| invalid_layout())?;
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
    active: &Active,
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
    {
        return Err(ProtocolFault::new(
            "InvalidMessage",
            "conflicting workspace or tab selection",
        ));
    }
    if let Some(label) = &target.new_workspace {
        let tab = target.new_tab.clone().unwrap_or_else(|| "Shell".to_owned());
        return Arc::make_mut(layout)
            .add_workspace(label.clone(), tab)
            .map_err(|_| invalid_layout());
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
        return Arc::make_mut(layout)
            .add_tab(location.workspace, label.clone())
            .map_err(|_| invalid_layout());
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

fn prepare_cwd(layout: &Layout, location: Location, launch: Option<&mut Launch>) -> Reply<()> {
    let Some(launch) = launch else { return Ok(()) };
    if launch.cwd.is_none() {
        launch.cwd = layout
            .last_cwd(location)
            .and_then(|cwd| crate::platform::decode_os_string(&cwd.path))
            .map(PathBuf::from)
            .filter(|path| path.is_dir());
    }
    if launch.cwd.as_ref().is_some_and(|path| !path.is_dir()) {
        return Err(ProtocolFault::new(
            "InvalidMessage",
            "launch directory does not exist",
        ));
    }
    Ok(())
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
