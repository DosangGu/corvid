use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use corvid_core::layout::{CwdObservation, CwdSource, Layout, Location};
use corvid_core::{InputRoute, Viewport};
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use uuid::Uuid;

use crate::terminal::{AttachSpec, Event, Launch, TabCommand, TerminalHandle};

const MAX_PTY_WORKERS: usize = 32;
const EVENT_CAPACITY: usize = 4;

type Reply<T> = Result<T, ProtocolFault>;

pub(crate) struct AttachTarget {
    pub(crate) workspace: Option<Uuid>,
    pub(crate) tab: Option<Uuid>,
    pub(crate) new_workspace: Option<String>,
    pub(crate) new_tab: Option<String>,
    pub(crate) tab_keys: bool,
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
}

pub(crate) enum Destination {
    Exact(Location),
    Tab(TabCommand),
}

pub(crate) struct Switched {
    pub(crate) active: Active,
    pub(crate) paste_discarded: bool,
}

pub(crate) struct Workspaces {
    registry: Mutex<Registry>,
    layouts: watch::Sender<Arc<Layout>>,
}

#[derive(Default)]
struct Registry {
    generation: u64,
    claims: HashMap<Uuid, Claim>,
    slots: HashMap<Uuid, Slot>,
    retired: Vec<JoinHandle<()>>,
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
    pub(crate) fn new(layouts: watch::Sender<Arc<Layout>>) -> Self {
        Self {
            registry: Mutex::new(Registry::default()),
            layouts,
        }
    }

    pub(crate) async fn any_live(&self) -> bool {
        self.registry
            .lock()
            .await
            .slots
            .values()
            .any(|slot| slot.terminal.has_live_terminal())
    }

    pub(crate) async fn attach(&self, client: Uuid, mut target: AttachTarget) -> Reply<Active> {
        let (location, generation, terminal) = self.reserve(client, &mut target).await?;
        let cwd = target.launch.as_ref().and_then(|launch| launch.cwd.clone());
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let spec = AttachSpec {
            client_id: client,
            generation,
            lease_generation: generation,
            revision: 1,
            dormant: false,
            tab_keys: target.tab_keys,
            viewport: target.viewport,
            launch: target.launch,
            events,
        };
        let opened = open_pane(terminal, location, spec, receiver).await;

        let mut registry = self.registry.lock().await;
        if let Some(slot) = registry.slots.get_mut(&location.pane) {
            slot.reserved = false;
        }
        match opened {
            Ok(active) => {
                if let Some(cwd) = cwd {
                    self.record_cwd(location, &cwd);
                }
                registry.resize_hidden(client, active.viewport, Some(active.location.pane));
                Ok(active)
            }
            Err(fault) => {
                registry.claims.retain(|_, claim| claim.client != client);
                Err(fault)
            }
        }
    }

    async fn reserve(
        &self,
        client: Uuid,
        target: &mut AttachTarget,
    ) -> Reply<(Location, u64, TerminalHandle)> {
        let mut registry = self.registry.lock().await;
        registry.sweep();
        let mut layout = Arc::clone(&self.layouts.borrow());
        let location = resolve(&registry, &mut layout, target)?;
        registry.check_available(location.workspace)?;
        prepare_cwd(&layout, location, target.launch.as_mut())?;
        let generation = registry.next_generation()?;
        let terminal = registry.reserve_slot(location, target.launch.is_some())?;
        if target.new_workspace.is_some() || target.new_tab.is_some() {
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

    pub(crate) async fn release(&self, client: Uuid) {
        self.registry
            .lock()
            .await
            .claims
            .retain(|_, claim| claim.client != client);
    }

    pub(crate) async fn claim(
        &self,
        client: Uuid,
        workspace: Uuid,
        viewport: Viewport,
    ) -> Reply<u64> {
        let mut registry = self.registry.lock().await;
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
        Ok(generation)
    }

    pub(crate) async fn resize_hidden(&self, active: &Active) {
        self.registry.lock().await.resize_hidden(
            active.route.client_id,
            active.viewport,
            Some(active.location.pane),
        );
    }

    pub(crate) async fn switch(
        &self,
        old: &Active,
        destination: Destination,
    ) -> Reply<Option<Switched>> {
        let Some((location, terminal, spec, events)) =
            self.reserve_switch(old, destination).await?
        else {
            return Ok(None);
        };
        let cwd = spec.launch.as_ref().and_then(|launch| launch.cwd.clone());
        let result = transition(old, terminal, location, spec, events).await;
        let mut registry = self.registry.lock().await;
        if let Some(slot) = registry.slots.get_mut(&location.pane) {
            slot.reserved = false;
        }
        if result.is_ok()
            && let Some(cwd) = cwd
        {
            self.record_cwd(location, &cwd);
        }
        result.map(Some)
    }

    async fn reserve_switch(
        &self,
        old: &Active,
        destination: Destination,
    ) -> Reply<Option<(Location, TerminalHandle, AttachSpec, mpsc::Receiver<Event>)>> {
        let mut registry = self.registry.lock().await;
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
        let revision = old.route.view_revision.checked_add(1).ok_or_else(limit)?;
        let terminal = registry.reserve_slot(location, launch.is_some())?;
        if launch.is_some() {
            self.layouts.send_replace(layout);
        }
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let spec = AttachSpec {
            client_id: old.route.client_id,
            generation: old.route.connection_generation,
            lease_generation,
            revision,
            dormant: true,
            tab_keys: old.tab_keys,
            viewport: old.viewport,
            launch,
            events,
        };
        Ok(Some((location, terminal, spec, receiver)))
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        let mut registry = self.registry.lock().await;
        let slots = std::mem::take(&mut registry.slots);
        let mut workers = std::mem::take(&mut registry.retired);
        drop(registry);
        for slot in slots.into_values() {
            workers.push(slot.task);
        }
        for worker in workers {
            worker.await?;
        }
        Ok(())
    }
}

impl Registry {
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

    fn reserve_slot(&mut self, location: Location, launch: bool) -> Reply<TerminalHandle> {
        if let Some(slot) = self.slots.get_mut(&location.pane) {
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
        if self.slots.len() + self.retired.len() >= MAX_PTY_WORKERS {
            return Err(limit());
        }
        let (terminal, task) = TerminalHandle::start();
        self.slots.insert(
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

async fn open_pane(
    terminal: TerminalHandle,
    location: Location,
    spec: AttachSpec,
    receiver: mpsc::Receiver<Event>,
) -> Reply<Active> {
    let viewport = spec.viewport;
    let tab_keys = spec.tab_keys;
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
