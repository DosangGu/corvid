mod controllers;
mod focus;
mod shell;

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use corvid_core::agent::{ProcessLiveness, Provider};
use corvid_core::layout::{CwdObservation, CwdSource, Layout, Location};
use corvid_core::{InputRoute, Viewport};
use corvid_protocol::wire::snapshot_entity::Body as EntityBody;
use corvid_protocol::wire::{
    AgentList, ClientKeymap, ClientList, ClientState, CollectorStatus, TabMoved, TerminalState,
    TuiInputMode, Viewport as WireViewport, WriterLeaseState,
};
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use uuid::Uuid;

use crate::agents::{Agents, Hook, HookBinding};
use crate::collector::{CollectorDiagnostics, HookEnvironment};
use crate::metadata::MetadataUpdates;
use crate::process::{self, HookProducer, Incarnation, ProbeBudget, ProcessImage};
use crate::projection;
use crate::publication::{Capture, Group, Projection, Publication};
use crate::terminal::{
    AttachSpec, Event, InputMode, Keymap, Launch, LaunchScope, TabCommand, TerminalHandle,
};

pub(crate) use controllers::ControllerCommand;
use controllers::Controllers;
pub(crate) use focus::{FocusDispatch, FocusRecovery};
use shell::Shells;

pub(crate) const MAX_PTY_WORKERS: usize = 32;
const EVENT_CAPACITY: usize = 4;

type Reply<T> = Result<T, ProtocolFault>;

pub(crate) struct AttachTarget {
    pub(crate) workspace: Option<Uuid>,
    pub(crate) tab: Option<Uuid>,
    pub(crate) new_workspace: Option<String>,
    pub(crate) new_tab: Option<String>,
    pub(crate) keymap: Keymap,
    pub(crate) viewport: Viewport,
    pub(crate) launch: Option<Launch>,
}

pub(crate) struct Active {
    pub(crate) location: Location,
    pub(crate) viewport: Viewport,
    pub(crate) route: InputRoute,
    pub(crate) terminal: TerminalHandle,
    pub(crate) events: mpsc::Receiver<Event>,
}

pub(crate) enum Destination {
    Exact(Location),
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
    boot_id: Option<Uuid>,
    probes: Arc<ProbeBudget>,
    hook_environment: Arc<HookEnvironment>,
}

pub(crate) struct Registry {
    pub(crate) agents: Agents,
    metadata: MetadataUpdates,
    epoch: Uuid,
    publication: Publication,
    terminals: HashMap<Uuid, Location>,
    shells: Shells,
    clients: HashMap<Uuid, Client>,
    controllers: Controllers,
    generation: u64,
    claims: HashMap<Uuid, Claim>,
    slots: HashMap<Uuid, Slot>,
    retired: Vec<JoinHandle<()>>,
}

struct Client {
    generation: u64,
    keymap: Keymap,
    view: Option<ClientView>,
    focuses: mpsc::Sender<FocusDispatch>,
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
        let epoch = Uuid::parse_str(&hook_environment.identity.epoch).expect("valid daemon epoch");
        Self {
            collector: CollectorDiagnostics::default(),
            registry: Arc::new(Mutex::new(Registry {
                agents: Agents::default(),
                metadata: metadata.clone(),
                epoch,
                publication,
                terminals: HashMap::new(),
                shells: Shells::default(),
                clients: HashMap::new(),
                controllers: Controllers::default(),
                generation: 0,
                claims: HashMap::new(),
                slots: HashMap::new(),
                retired: Vec::new(),
            })),
            metadata,
            boot_id: process::boot_id(),
            probes: Arc::new(ProbeBudget::default()),
            hook_environment,
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

    pub(crate) fn tick_agents(&self) {
        self.poll_shell_agents();
        let mut registry = self.lock_registry();
        registry.agents.tick();
        registry.publish(Vec::new());
    }

    pub(crate) fn hook_binding(&self, association: Uuid) -> HookBinding {
        self.lock_registry().agents.binding(association)
    }

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
            && let Some(claim) = registry.claims.get(&location.workspace)
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
        let mut collector = registry.agents.collector_status();
        collector.traffic = Some(self.collector.traffic());
        (terminal_live, collector)
    }

    pub(crate) async fn attach(
        &self,
        client: Uuid,
        mut target: AttachTarget,
        focuses: mpsc::Sender<FocusDispatch>,
        notices: Option<mpsc::Sender<Provider>>,
    ) -> Reply<Active> {
        let (location, generation, terminal) = self.reserve(client, &mut target)?;
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let spec = AttachSpec {
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
        let opened = open_pane(terminal, location, spec, receiver).await;

        let mut registry = self.lock_registry();
        if let Some(slot) = registry.slots.get_mut(&location.pane) {
            slot.reserved = false;
        }
        match opened {
            Ok(active) => {
                registry.resize_hidden(client, active.viewport, Some(active.location.pane));
                registry.clients.insert(
                    client,
                    Client {
                        generation,
                        keymap: target.keymap,
                        view: None,
                        focuses,
                        notices,
                    },
                );
                registry.record_view(&active);
                Ok(active)
            }
            Err(fault) => {
                registry.claims.retain(|_, claim| claim.client != client);
                Err(fault)
            }
        }
    }

    fn reserve(
        &self,
        client: Uuid,
        target: &mut AttachTarget,
    ) -> Reply<(Location, u64, TerminalHandle)> {
        let mut registry = self.lock_registry();
        registry.sweep();
        let mut layout = self.metadata.layout();
        let location = resolve(&registry, &mut layout, target)?;
        registry.check_available(location.workspace)?;
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
        registry
            .claims
            .insert(location.workspace, Claim { client, generation });
        Ok((location, generation, terminal))
    }

    pub(crate) fn release(&self, client: Uuid) {
        let mut registry = self.lock_registry();
        registry.claims.retain(|_, claim| claim.client != client);
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
        if let Some(claim) = registry.claims.get(&workspace) {
            return if claim.client == client {
                Ok(claim.generation)
            } else {
                Err(busy())
            };
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
        registry.require_writer(client, source_workspace)?;
        registry.require_writer(client, destination)?;

        let mut layout = self.metadata.layout();
        let source = layout
            .locate(Some(source_workspace), Some(tab))
            .ok_or_else(target_gone)?;
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
            active.viewport,
            Some(active.location.pane),
        );
        registry.record_view(active);
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
        let lease_generation = registry.require_writer(old.route.client_id, location.workspace)?;
        let reserved = self.reserve_view(&mut registry, old, location, lease_generation, launch)?;
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
        let terminal = self.reserve_slot(registry, location, launch.is_some())?;
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let spec = AttachSpec {
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
        let scope = LaunchScope {
            location,
            registry: Arc::downgrade(&self.registry),
            boot_id: self.boot_id,
            probes: Arc::clone(&self.probes),
            hook_environment: Arc::clone(&self.hook_environment),
        };
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
        self.metadata
            .update(layout, self.agents.changed_history(self.epoch));
        projections.extend(self.agents.take_projections());
        self.publication.commit(projections);
    }

    pub(crate) fn terminal_started(&mut self, id: Uuid, location: Location) {
        self.terminals.insert(id, location);
        self.publish(vec![terminal_projection(id, location)]);
    }

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
        if let Some(association) = association {
            self.agents.retire_launch(association);
        }
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
        let route = active.route;
        let view = self
            .terminals
            .contains_key(&route.terminal_id)
            .then_some(ClientView {
                route,
                location: active.location,
                viewport: active.viewport,
                input_mode: InputMode::Passthrough,
            });
        if let Some(client) = self.clients.get_mut(&route.client_id)
            && client.generation == route.connection_generation
        {
            client.view = view;
            self.publish_client(route.client_id);
        }
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
        Some(ClientState {
            client_id: id.to_string(),
            connection_generation: client.generation,
            terminal_id: view.map(|view| view.route.terminal_id.to_string()),
            selection: view.map(|view| projection::selection(view.location)),
            view_revision: view.map(|view| view.route.view_revision),
            viewport: view.map(|view| WireViewport {
                cols: u32::from(view.viewport.cols.get()),
                rows: u32::from(view.viewport.rows.get()),
            }),
            writer_leases,
            bindings: self.controllers.bindings(id),
            input_mode: view.map(|view| match view.input_mode {
                InputMode::Passthrough => TuiInputMode::Passthrough as i32,
                InputMode::Command => TuiInputMode::Command as i32,
            }),
            keymap: Some(ClientKeymap {
                tabs: client.keymap.tabs,
                pairing: client.keymap.pairing,
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

    fn check_available(&self, workspace: Uuid) -> Reply<()> {
        if self.claims.contains_key(&workspace) {
            return Err(busy());
        }
        Ok(())
    }

    fn require_writer(&self, client: Uuid, workspace: Uuid) -> Reply<u64> {
        let claim = self.claims.get(&workspace).ok_or_else(writer_required)?;
        if claim.client != client {
            return Err(busy());
        }
        Ok(claim.generation)
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
        Destination::Exact(location) => {
            if layout.locate(Some(location.workspace), Some(location.tab)) != Some(location) {
                return Err(target_gone());
            }
            Ok(Some((location, None)))
        }
        Destination::Tab(TabCommand::Create) => {
            let workspace = layout
                .workspaces
                .iter()
                .find(|item| item.id == old.location.workspace)
                .ok_or_else(target_gone)?;
            let label = format!("Shell {}", workspace.tabs.len() + 1);
            let mut launch = Launch {
                argv: Vec::new(),
                cwd: None,
                provider: None,
                if_empty: false,
            };
            prepare_cwd(layout, old.location, Some(&mut launch))?;
            let location = Arc::make_mut(layout)
                .add_tab(old.location.workspace, label)
                .map_err(|_| invalid_layout())?;
            Ok(Some((location, Some(launch))))
        }
        Destination::Tab(command) => {
            Ok(cycle(registry, layout, old.location, command).map(|location| (location, None)))
        }
    }
}

fn cycle(
    registry: &Registry,
    layout: &Layout,
    old: Location,
    command: TabCommand,
) -> Option<Location> {
    let workspace = layout
        .workspaces
        .iter()
        .find(|item| item.id == old.workspace)?;
    let index = workspace.tabs.iter().position(|tab| tab.id == old.tab)?;
    let len = workspace.tabs.len();
    (1..len).find_map(|offset| {
        let index = match command {
            TabCommand::Next => (index + offset) % len,
            TabCommand::Previous => (index + len - offset) % len,
            TabCommand::Create => return None,
        };
        let location = layout.locate(Some(old.workspace), Some(workspace.tabs[index].id))?;
        registry
            .slots
            .get(&location.pane)
            .filter(|slot| slot.terminal.has_live_terminal())
            .map(|_| location)
    })
}

fn resolve(
    registry: &Registry,
    layout: &mut Arc<Layout>,
    target: &AttachTarget,
) -> Reply<Location> {
    if (target.new_workspace.is_some() && (target.workspace.is_some() || target.tab.is_some()))
        || (target.new_tab.is_some() && target.tab.is_some())
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
    registry.check_available(location.workspace)?;
    if let Some(label) = &target.new_tab {
        return Arc::make_mut(layout)
            .add_tab(location.workspace, label.clone())
            .map_err(|_| invalid_layout());
    }
    if target.tab.is_none() && target.launch.as_ref().is_none_or(|launch| launch.if_empty) {
        let workspace = layout
            .workspaces
            .iter()
            .find(|item| item.id == location.workspace);
        if let Some(live) = workspace
            .into_iter()
            .flat_map(|item| &item.tabs)
            .filter_map(|tab| layout.locate(Some(location.workspace), Some(tab.id)))
            .find(|location| registry.has_live_at(*location))
        {
            return Ok(live);
        }
    }
    Ok(location)
}

fn prepare_cwd(layout: &Layout, location: Location, launch: Option<&mut Launch>) -> Reply<()> {
    let Some(launch) = launch else { return Ok(()) };
    if launch.cwd.is_none() {
        launch.cwd = layout
            .last_cwd(location)
            .map(|cwd| PathBuf::from(OsString::from_vec(cwd.path.clone())))
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

fn busy() -> ProtocolFault {
    ProtocolFault::new("WorkspaceBusy", "workspace already has a writer")
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
