mod controllers;
mod focus;

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use corvid_core::layout::{CwdObservation, CwdSource, Layout, Location};
use corvid_core::{InputRoute, Viewport};
use corvid_protocol::wire::snapshot_entity::Body as EntityBody;
use corvid_protocol::wire::{
    AgentList, ClientList, ClientState, TerminalState, Viewport as WireViewport, WriterLeaseState,
};
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use uuid::Uuid;

use crate::agents::{Agents, Hook, HookBinding};
use crate::collector::HookEnvironment;
use crate::process::{self, HookProducer, ProbeBudget, ProcessImage};
use crate::projection;
use crate::publication::{Capture, Group, Projection, Publication};
use crate::terminal::{AttachSpec, Event, Launch, LaunchScope, TabCommand, TerminalHandle};

pub(crate) use controllers::ControllerCommand;
use controllers::Controllers;
pub(crate) use focus::{FocusDispatch, FocusRecovery};

pub(crate) const MAX_PTY_WORKERS: usize = 32;
const EVENT_CAPACITY: usize = 4;

type Reply<T> = Result<T, ProtocolFault>;

pub(crate) struct AttachTarget {
    pub(crate) workspace: Option<Uuid>,
    pub(crate) tab: Option<Uuid>,
    pub(crate) new_workspace: Option<String>,
    pub(crate) new_tab: Option<String>,
    pub(crate) tab_keys: bool,
    pub(crate) pair_keys: bool,
    pub(crate) viewport: Viewport,
    pub(crate) launch: Option<Launch>,
}

pub(crate) struct Active {
    pub(crate) location: Location,
    pub(crate) viewport: Viewport,
    pub(crate) route: InputRoute,
    pub(crate) terminal: TerminalHandle,
    pub(crate) events: mpsc::Receiver<Event>,
    tab_keys: bool,
    pair_keys: bool,
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
    registry: Arc<Mutex<Registry>>,
    layouts: watch::Sender<Arc<Layout>>,
    boot_id: Option<Uuid>,
    probes: Arc<ProbeBudget>,
    hook_environment: Arc<HookEnvironment>,
}

#[derive(Default)]
pub(crate) struct Registry {
    pub(crate) agents: Agents,
    publication: Publication,
    terminals: HashMap<Uuid, Location>,
    clients: HashMap<Uuid, Client>,
    controllers: Controllers,
    generation: u64,
    claims: HashMap<Uuid, Claim>,
    slots: HashMap<Uuid, Slot>,
    retired: Vec<JoinHandle<()>>,
}

struct Client {
    generation: u64,
    view: Option<ClientView>,
    focuses: mpsc::Sender<FocusDispatch>,
}

struct ClientView {
    route: InputRoute,
    location: Location,
    viewport: Viewport,
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
    pub(crate) fn new(
        layouts: watch::Sender<Arc<Layout>>,
        hook_environment: Arc<HookEnvironment>,
    ) -> Self {
        let publication = Publication::seed(projection::layout(&layouts.borrow()));
        Self {
            registry: Arc::new(Mutex::new(Registry {
                publication,
                ..Registry::default()
            })),
            layouts,
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
        registry.agents.apply_hook(hook, owner, producer)?;
        registry.publish(Vec::new());
        Ok(())
    }

    pub(crate) fn any_live(&self) -> bool {
        self.lock_registry()
            .slots
            .values()
            .any(|slot| slot.terminal.has_live_terminal())
    }

    pub(crate) async fn attach(
        &self,
        client: Uuid,
        mut target: AttachTarget,
        focuses: mpsc::Sender<FocusDispatch>,
    ) -> Reply<Active> {
        let (location, generation, terminal) = self.reserve(client, &mut target)?;
        let cwd = target.launch.as_ref().and_then(|launch| launch.cwd.clone());
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let spec = AttachSpec {
            client_id: client,
            generation,
            lease_generation: generation,
            revision: 1,
            dormant: false,
            tab_keys: target.tab_keys,
            pair_keys: target.pair_keys,
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
                if let Some(cwd) = cwd {
                    self.record_cwd(location, &cwd);
                }
                registry.resize_hidden(client, active.viewport, Some(active.location.pane));
                registry.clients.insert(
                    client,
                    Client {
                        generation,
                        view: None,
                        focuses,
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
        let mut layout = Arc::clone(&self.layouts.borrow());
        let location = resolve(&registry, &mut layout, target)?;
        registry.check_available(location.workspace)?;
        prepare_cwd(&layout, location, target.launch.as_mut())?;
        let generation = registry.next_generation()?;
        let terminal = self.reserve_slot(&mut registry, location, target.launch.is_some())?;
        if target.new_workspace.is_some() || target.new_tab.is_some() {
            registry.publish(vec![projection::layout(&layout)]);
            self.layouts.send_replace(layout);
        }
        registry
            .claims
            .insert(location.workspace, Claim { client, generation });
        Ok((location, generation, terminal))
    }

    fn record_cwd(&self, location: Location, cwd: &Path) {
        let mut layout = Arc::clone(&self.layouts.borrow());
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let observation = CwdObservation {
            path: cwd.as_os_str().as_encoded_bytes().to_vec(),
            observed_unix_ms: u64::try_from(time.as_millis()).unwrap_or(u64::MAX),
            source: CwdSource::LaunchDirectory,
        };
        if Arc::make_mut(&mut layout)
            .record_cwd(location, observation)
            .is_ok()
        {
            self.layouts.send_replace(layout);
        }
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
            .layouts
            .borrow()
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
        let cwd = spec.launch.as_ref().and_then(|launch| launch.cwd.clone());
        let result = transition(old, terminal, location, spec, events).await;
        let mut registry = self.lock_registry();
        if let Some(slot) = registry.slots.get_mut(&location.pane) {
            slot.reserved = false;
        }
        if result.is_ok()
            && let Some(cwd) = cwd
        {
            self.record_cwd(location, &cwd);
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
        registry.sweep();
        let mut layout = Arc::clone(&self.layouts.borrow());
        let Some((location, launch)) = resolve_switch(&registry, &mut layout, old, destination)?
        else {
            return Ok(None);
        };
        if location == old.location {
            return Ok(None);
        }
        let claim = registry
            .claims
            .get(&location.workspace)
            .ok_or_else(writer_required)?;
        if claim.client != old.route.client_id {
            return Err(busy());
        }
        let lease_generation = claim.generation;
        let reserved = self.reserve_view(&mut registry, old, location, lease_generation, launch)?;
        if reserved.spec.launch.is_some() {
            registry.publish(vec![projection::layout(&layout)]);
            self.layouts.send_replace(layout);
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
        let terminal = self.reserve_slot(registry, location, launch.is_some())?;
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let spec = AttachSpec {
            client_id: old.route.client_id,
            generation: old.route.connection_generation,
            lease_generation,
            revision,
            dormant: true,
            tab_keys: old.tab_keys,
            pair_keys: old.pair_keys,
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
    pub(crate) fn publish(&mut self, mut projections: Vec<Projection>) {
        projections.extend(self.agents.take_projections());
        self.publication.commit(projections);
    }

    pub(crate) fn terminal_started(&mut self, id: Uuid, location: Location) {
        self.terminals.insert(id, location);
        let state = TerminalState {
            terminal_id: id.to_string(),
            location: Some(projection::selection(location)),
        };
        self.publish(vec![(
            Group::Terminal(id),
            vec![projection::entry(EntityBody::Terminal(state))],
        )]);
    }

    pub(crate) fn terminal_closed(&mut self, id: Uuid, association: Option<Uuid>) {
        if let Some(association) = association {
            self.agents.retire_launch(association);
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
            });
        if let Some(client) = self.clients.get_mut(&route.client_id)
            && client.generation == route.connection_generation
        {
            client.view = view;
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

    fn next_generation(&mut self) -> Reply<u64> {
        self.generation = self.generation.checked_add(1).ok_or_else(limit)?;
        Ok(self.generation)
    }
}

async fn open_pane(
    terminal: TerminalHandle,
    location: Location,
    spec: AttachSpec,
    receiver: mpsc::Receiver<Event>,
) -> Reply<Active> {
    let viewport = spec.viewport;
    let tab_keys = spec.tab_keys;
    let pair_keys = spec.pair_keys;
    let route = timeout(COMMAND_TIMEOUT, terminal.attach(spec))
        .await
        .map_err(|_| ProtocolFault::new("Timeout", "terminal attach timed out"))??;
    Ok(Active {
        location,
        viewport,
        route,
        terminal,
        events: receiver,
        tab_keys,
        pair_keys,
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
    if target.tab.is_none() && target.launch.is_none() {
        let workspace = layout
            .workspaces
            .iter()
            .find(|item| item.id == location.workspace);
        if let Some(live) = workspace
            .into_iter()
            .flat_map(|item| &item.tabs)
            .filter_map(|tab| layout.locate(Some(location.workspace), Some(tab.id)))
            .find(|location| {
                registry
                    .slots
                    .get(&location.pane)
                    .is_some_and(|slot| slot.terminal.has_live_terminal())
            })
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
