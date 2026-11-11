use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::Result;
#[cfg(target_os = "linux")]
use corvid_core::agent::Source;
use corvid_core::agent::{ProcessLiveness, Provider};
use corvid_core::layout::{CwdSource, Layout, Location};
use corvid_core::{InputRoute, KEY_LEADER, Viewport};
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault, RENDER_LIMIT};
use portable_pty::{CommandBuilder, ExitStatus};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;
use vt100::Screen;

#[cfg(target_os = "linux")]
use crate::agents::ManagedLaunch;
use crate::collector::HookEnvironment;
use crate::metadata::MetadataUpdates;
#[cfg(target_os = "linux")]
use crate::process::{
    self, Executable, Incarnation, Probe, ProbeBudget, ProbeResult, ProcessImage,
};
#[cfg(target_os = "linux")]
use crate::provider;
use crate::workspaces::Registry;

use super::backend::Backend;
use super::host::HostConfig;
use super::input::{Kind, WriteFailure};
use super::keys::{InputMode, KeyCommand, Keymap, TabCommand};
use super::paste::Paste;
use super::status::{self, TuiViewport};
use super::tabs::{PickerAction, TabPicker};

const COMMAND_CAPACITY: usize = 32;
const TICK: Duration = Duration::from_millis(8);
const FRAME_INTERVAL: Duration = Duration::from_millis(34);
const MAX_ANSI: usize = RENDER_LIMIT - 16 * 1024 - 5;
const LEADER_TIMEOUT: Duration = Duration::from_secs(2);

type Reply<T> = Result<T, ProtocolFault>;

pub(crate) struct Launch {
    pub(crate) argv: Vec<OsString>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) provider: Option<Provider>,
    pub(crate) if_empty: bool,
}

pub(crate) struct LaunchScope {
    pub(crate) location: Location,
    pub(crate) metadata: MetadataUpdates,
    pub(crate) registry: Weak<Mutex<Registry>>,
    #[cfg(target_os = "linux")]
    pub(crate) boot_id: Option<Uuid>,
    #[cfg(target_os = "linux")]
    pub(crate) probes: Arc<ProbeBudget>,
    pub(crate) hook_environment: Arc<HookEnvironment>,
    pub(crate) host: Option<HostConfig>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub(crate) struct RetainedTerminal {
    pub(crate) id: Uuid,
    shell: bool,
    #[cfg(windows)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<Provider>,
}

impl LaunchScope {
    fn tab_picker(&self, location: Location) -> Option<TabPicker> {
        let registry = self.registry.upgrade()?;
        let registry = registry.lock().ok()?;
        let layout = self.metadata.layout();
        let workspace = layout
            .workspaces
            .iter()
            .find(|workspace| workspace.id == location.workspace)?;
        TabPicker::new(workspace, location.tab, |tab| {
            registry.live_tab(&layout, workspace.id, tab).is_some()
        })
    }
}

pub(crate) struct AttachSpec {
    pub(crate) location: Location,
    pub(crate) client_id: Uuid,
    pub(crate) generation: u64,
    pub(crate) lease_generation: u64,
    pub(crate) revision: u64,
    pub(crate) dormant: bool,
    pub(crate) keymap: Keymap,
    pub(crate) viewport: TuiViewport,
    pub(crate) launch: Option<Launch>,
    pub(crate) events: mpsc::Sender<Event>,
}

pub(crate) enum Action {
    Input {
        route: InputRoute,
        sequence: u64,
        bytes: Vec<u8>,
    },
    PasteBegin {
        route: InputRoute,
        sequence: u64,
        id: Uuid,
    },
    PasteChunk {
        id: Uuid,
        bytes: Vec<u8>,
    },
    PasteEnd {
        id: Uuid,
    },
    Resize {
        connection_generation: u64,
        viewport: Viewport,
    },
    FrameApplied {
        generation: u64,
        frame_id: u64,
    },
    Redraw {
        view_revision: u64,
    },
    Ping,
    Rejected(ProtocolFault),
}

pub(crate) enum Event {
    Render(Render),
    ViewChanged {
        route: InputRoute,
        viewport: Viewport,
    },
    Exited {
        terminal_id: Uuid,
        status: ExitStatus,
    },
    Error {
        request_id: Option<String>,
        fault: ProtocolFault,
    },
    Pong {
        request_id: Option<String>,
    },
    Detached,
    Tab(TabCommand),
    Close(corvid_core::layout::CloseTarget),
    PairController,
}

pub(crate) struct Render {
    pub(crate) route: InputRoute,
    pub(crate) generation: u64,
    pub(crate) frame_id: u64,
    pub(crate) base_frame_id: Option<u64>,
    pub(crate) ansi: Vec<u8>,
}

enum Command {
    Attach(AttachSpec, oneshot::Sender<Reply<InputRoute>>),
    Action {
        client: Uuid,
        request_id: Option<String>,
        action: Action,
    },
    Fence(InputRoute, oneshot::Sender<Reply<bool>>),
    Freeze(InputRoute, oneshot::Sender<Reply<()>>),
    Thaw(InputRoute, oneshot::Sender<Reply<InputRoute>>),
    Activate(InputRoute, TuiViewport, oneshot::Sender<Reply<()>>),
    Resume(InputRoute),
    Preview(oneshot::Sender<Reply<(Viewport, Vec<u8>)>>),
    History(
        InputRoute,
        oneshot::Sender<Reply<corvid_protocol::wire::TerminalHistory>>,
    ),
    Retain(bool, oneshot::Sender<Reply<Option<RetainedTerminal>>>),
}

#[derive(Clone)]
pub(crate) struct TerminalHandle {
    commands: mpsc::Sender<Command>,
    live: Arc<AtomicBool>,
    viewing: Arc<AtomicBool>,
    hidden_viewport: Arc<Mutex<Option<Viewport>>>,
}

impl TerminalHandle {
    pub(crate) fn start(scope: LaunchScope) -> (Self, JoinHandle<()>) {
        Self::start_worker(scope, None)
    }

    pub(crate) async fn recover(
        scope: LaunchScope,
        saved: RetainedTerminal,
    ) -> Result<(Self, JoinHandle<()>)> {
        let (scope, terminal) = tokio::task::spawn_blocking(move || {
            let terminal = Running::recover(saved, &scope)?;
            Ok::<_, anyhow::Error>((scope, terminal))
        })
        .await??;
        Ok(Self::start_worker(scope, Some(terminal)))
    }

    fn start_worker(scope: LaunchScope, terminal: Option<Running>) -> (Self, JoinHandle<()>) {
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let live = Arc::new(AtomicBool::new(terminal.is_some()));
        let viewing = Arc::new(AtomicBool::new(false));
        let hidden_viewport = Arc::new(Mutex::new(None));
        let mut worker = Worker {
            scope,
            terminal,
            lease: None,
            live: Arc::clone(&live),
            viewing: Arc::clone(&viewing),
            hidden_viewport: Arc::clone(&hidden_viewport),
            retaining: false,
        };
        let worker = tokio::task::spawn_blocking(move || worker.run(receiver));
        (
            Self {
                commands,
                live,
                viewing,
                hidden_viewport,
            },
            worker,
        )
    }

    pub(crate) fn has_live_terminal(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }

    pub(crate) fn has_view(&self) -> bool {
        self.viewing.load(Ordering::Acquire)
    }

    pub(crate) fn resize_hidden(&self, viewport: Viewport) {
        if let Ok(mut pending) = self.hidden_viewport.lock() {
            *pending = Some(viewport);
        }
    }

    pub(crate) async fn attach(&self, spec: AttachSpec) -> Reply<InputRoute> {
        let (send, receive) = oneshot::channel();
        self.commands
            .try_send(Command::Attach(spec, send))
            .map_err(|_| overloaded())?;
        receive.await.map_err(|_| overloaded())?
    }

    pub(crate) async fn preview(&self) -> Reply<(Viewport, Vec<u8>)> {
        let (send, receive) = oneshot::channel();
        self.commands
            .try_send(Command::Preview(send))
            .map_err(|_| overloaded())?;
        timeout_reply(receive).await
    }

    pub(crate) async fn history(
        &self,
        route: InputRoute,
    ) -> Reply<corvid_protocol::wire::TerminalHistory> {
        let (send, receive) = oneshot::channel();
        self.commands
            .try_send(Command::History(route, send))
            .map_err(|_| overloaded())?;
        timeout_reply(receive).await
    }

    pub(crate) async fn retain(&self, retain: bool) -> Reply<Option<RetainedTerminal>> {
        let (send, receive) = oneshot::channel();
        self.commands
            .try_send(Command::Retain(retain, send))
            .map_err(|_| overloaded())?;
        timeout_reply(receive).await
    }

    pub(crate) async fn fence(&self, route: InputRoute) -> Reply<bool> {
        let (send, receive) = oneshot::channel();
        self.commands
            .try_send(Command::Fence(route, send))
            .map_err(|_| overloaded())?;
        timeout_reply(receive).await
    }

    pub(crate) async fn freeze(&self, route: InputRoute) -> Reply<()> {
        let (send, receive) = oneshot::channel();
        self.commands
            .try_send(Command::Freeze(route, send))
            .map_err(|_| overloaded())?;
        timeout_reply(receive).await
    }

    pub(crate) async fn thaw(&self, route: InputRoute) -> Reply<InputRoute> {
        let (send, receive) = oneshot::channel();
        self.commands
            .try_send(Command::Thaw(route, send))
            .map_err(|_| overloaded())?;
        timeout_reply(receive).await
    }

    pub(crate) async fn activate(&self, route: InputRoute, viewport: TuiViewport) -> Reply<()> {
        let (send, receive) = oneshot::channel();
        self.commands
            .try_send(Command::Activate(route, viewport, send))
            .map_err(|_| overloaded())?;
        timeout_reply(receive).await
    }

    pub(crate) async fn resume(&self, route: InputRoute) -> Reply<()> {
        tokio::time::timeout(COMMAND_TIMEOUT, self.commands.send(Command::Resume(route)))
            .await
            .map_err(|_| overloaded())?
            .map_err(|_| terminal_fault())
    }

    pub(crate) async fn action(
        &self,
        client: Uuid,
        request_id: Option<String>,
        action: Action,
    ) -> Reply<()> {
        tokio::time::timeout(
            COMMAND_TIMEOUT,
            self.commands.send(Command::Action {
                client,
                request_id,
                action,
            }),
        )
        .await
        .map_err(|_| overloaded())?
        .map_err(|_| overloaded())
    }
}

fn overloaded() -> ProtocolFault {
    ProtocolFault::new("LimitExceeded", "terminal command queue is unavailable")
}

async fn timeout_reply<T>(reply: oneshot::Receiver<Reply<T>>) -> Reply<T> {
    tokio::time::timeout(COMMAND_TIMEOUT, reply)
        .await
        .map_err(|_| ProtocolFault::new("Timeout", "terminal transition timed out"))?
        .map_err(|_| terminal_fault())?
}

struct Worker {
    scope: LaunchScope,
    terminal: Option<Running>,
    lease: Option<Lease>,
    live: Arc<AtomicBool>,
    viewing: Arc<AtomicBool>,
    hidden_viewport: Arc<Mutex<Option<Viewport>>>,
    retaining: bool,
}

impl Worker {
    fn run(&mut self, mut commands: mpsc::Receiver<Command>) {
        loop {
            if self.terminal.is_none() {
                let Some(command) = commands.blocking_recv() else {
                    return;
                };
                self.handle(command);
            }
            for _ in 0..COMMAND_CAPACITY {
                match commands.try_recv() {
                    Ok(command) => self.handle(command),
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => return,
                }
            }
            let mode = self.input_mode();
            self.advance();
            self.publish_mode_change(mode);
            if self.terminal.is_some() {
                std::thread::sleep(TICK);
            }
        }
    }

    fn handle(&mut self, command: Command) {
        self.revoke_if_disconnected();
        let mode = self.input_mode();
        match command {
            Command::Attach(spec, reply) if !reply.is_closed() => {
                let _ = reply.send(self.attach(spec));
            }
            Command::Attach(_, _) => {}
            Command::Fence(route, reply) => {
                if !reply.is_closed() {
                    let _ = reply.send(self.fence(route));
                }
            }
            Command::Freeze(route, reply) => {
                if !reply.is_closed() {
                    let _ = reply.send(self.freeze(route));
                }
            }
            Command::Thaw(route, reply) => {
                if !reply.is_closed() {
                    let _ = reply.send(self.thaw(route));
                }
            }
            Command::Activate(route, viewport, reply) => {
                if !reply.is_closed() {
                    let _ = reply.send(self.activate(route, viewport));
                }
            }
            Command::Resume(route) => {
                if let Some(lease) = &mut self.lease
                    && lease.route == route
                {
                    lease.switch_pending = false;
                }
            }
            Command::Preview(reply) => {
                if !reply.is_closed() {
                    let _ = reply.send(self.preview());
                }
            }
            Command::History(route, reply) => {
                if !reply.is_closed() {
                    let _ = reply.send(self.history(route));
                }
            }
            Command::Retain(retain, reply) => {
                if !reply.is_closed() {
                    let _ = reply.send(self.retain(retain));
                }
            }
            Command::Action {
                client,
                request_id,
                action,
            } => {
                if let Err(fault) = self.apply(client, request_id.clone(), action)
                    && let Some(lease) = &self.lease
                    && lease.route.client_id == client
                    && lease
                        .events
                        .try_send(Event::Error { request_id, fault })
                        .is_err()
                {
                    self.revoke();
                }
            }
        }
        self.publish_mode_change(mode);
    }

    fn input_mode(&self) -> Option<InputMode> {
        self.lease.as_ref().map(Lease::input_mode)
    }

    fn preview(&mut self) -> Reply<(Viewport, Vec<u8>)> {
        let running = self
            .terminal
            .as_mut()
            .filter(|terminal| terminal.exit.is_none())
            .ok_or_else(terminal_fault)?;
        let (viewport, ansi) = running.session.preview().map_err(|_| terminal_fault())?;
        if ansi.len() > MAX_ANSI {
            return Err(terminal_fault());
        }
        Ok((viewport, ansi))
    }

    fn history(&mut self, route: InputRoute) -> Reply<corvid_protocol::wire::TerminalHistory> {
        if self
            .lease
            .as_ref()
            .is_none_or(|lease| lease.route != route || lease.switch_pending)
        {
            return Err(stale_route());
        }
        let terminal = self
            .terminal
            .as_mut()
            .filter(|terminal| terminal.exit.is_none())
            .ok_or_else(terminal_fault)?;
        terminal.session.history().map_err(|_| terminal_fault())
    }

    fn publish_mode_change(&self, previous: Option<InputMode>) {
        let Some(lease) = self
            .lease
            .as_ref()
            .filter(|lease| lease.armed && !lease.frozen)
        else {
            return;
        };
        let mode = lease.input_mode();
        if previous.is_some_and(|previous| previous != mode)
            && let Some(registry) = self.scope.registry.upgrade()
            && let Ok(mut registry) = registry.lock()
        {
            registry.record_input_mode(lease.route, mode);
        }
    }

    fn retain(&mut self, retain: bool) -> Reply<Option<RetainedTerminal>> {
        if !retain && !self.retaining {
            return Ok(None);
        }
        let Some(terminal) = self
            .terminal
            .as_mut()
            .filter(|terminal| terminal.exit.is_none())
        else {
            return Ok(None);
        };
        if retain
            && (terminal.session.paste_pending()
                || self.lease.as_ref().is_some_and(|lease| {
                    lease.paste.is_some()
                        || lease.switch_pending
                        || (lease.frozen && !self.retaining)
                }))
        {
            return Err(input_busy());
        }
        let live = terminal
            .session
            .retain(retain)
            .map_err(|_| terminal_fault())?;
        if retain && !live {
            return Err(ProtocolFault::new(
                "DaemonBusy",
                "a live terminal cannot be handed to the replacement daemon",
            ));
        }
        if let Some(lease) = &mut self.lease {
            lease.frozen = retain;
            lease.leader = None;
            lease.picker = None;
        }
        self.retaining = retain;
        Ok(live.then_some(RetainedTerminal {
            id: terminal.id,
            shell: terminal.shell,
            #[cfg(windows)]
            provider: terminal.provider,
        }))
    }

    fn attach(&mut self, spec: AttachSpec) -> Reply<InputRoute> {
        if self.lease.is_some() {
            return Err(ProtocolFault::new(
                "WorkspaceBusy",
                "workspace already has a writer",
            ));
        }
        let content = spec.viewport.content();
        let launch = spec
            .launch
            .filter(|launch| !launch.if_empty || self.terminal.is_none());
        match (&mut self.terminal, launch) {
            (Some(terminal), None) if terminal.exit.is_some() => return Err(terminal_fault()),
            (Some(_), None) if spec.dormant => {}
            (Some(terminal), None) => terminal.resize(content).map_err(|_| terminal_fault())?,
            (Some(_), Some(_)) => {
                return Err(ProtocolFault::new(
                    "TerminalExists",
                    "attach to the existing terminal",
                ));
            }
            (None, Some(launch)) => {
                self.terminal =
                    Some(Running::spawn(launch, content, &self.scope).map_err(|_| {
                        ProtocolFault::new(
                            "TerminalStartFailed",
                            "unable to start the requested terminal",
                        )
                    })?);
                self.live.store(true, Ordering::Release);
            }
            (None, None) => {
                return Err(ProtocolFault::new(
                    "NoTerminal",
                    "use corvid attach --new to start a terminal",
                ));
            }
        }
        let terminal = self.terminal.as_mut().ok_or_else(terminal_fault)?;
        terminal.dirty = true;
        let route = InputRoute {
            client_id: spec.client_id,
            terminal_id: terminal.id,
            connection_generation: spec.generation,
            view_revision: spec.revision,
            writer_lease_generation: spec.lease_generation,
        };
        self.lease = Some(Lease::new(
            route,
            spec.events,
            !spec.dormant,
            spec.keymap,
            spec.viewport,
            spec.location,
        ));
        self.viewing.store(true, Ordering::Release);
        Ok(route)
    }

    fn fence(&mut self, route: InputRoute) -> Reply<bool> {
        let lease = self
            .lease
            .as_ref()
            .filter(|lease| lease.armed && lease.route == route)
            .ok_or_else(stale_route)?;
        let terminal = self.terminal.as_ref().ok_or_else(terminal_fault)?;
        if terminal.session.paste_pending() {
            return Err(input_busy());
        }
        let discarded = lease.paste.is_some();
        self.revoke();
        Ok(discarded)
    }

    fn freeze(&mut self, route: InputRoute) -> Reply<()> {
        let lease = self
            .lease
            .as_mut()
            .filter(|lease| lease.armed && lease.route == route && !lease.frozen)
            .ok_or_else(stale_route)?;
        let terminal = self
            .terminal
            .as_mut()
            .filter(|terminal| terminal.exit.is_none())
            .ok_or_else(terminal_fault)?;
        if lease.switch_pending || lease.paste.is_some() || terminal.session.paste_pending() {
            return Err(input_busy());
        }
        terminal.fence_input().map_err(|_| input_busy())?;
        lease.frozen = true;
        lease.leader = None;
        lease.picker = None;
        Ok(())
    }

    fn thaw(&mut self, route: InputRoute) -> Reply<InputRoute> {
        let lease = self
            .lease
            .as_mut()
            .filter(|lease| lease.armed && lease.route == route && lease.frozen)
            .ok_or_else(stale_route)?;
        let terminal = self
            .terminal
            .as_mut()
            .filter(|terminal| terminal.exit.is_none())
            .ok_or_else(terminal_fault)?;
        let revision = route.view_revision.checked_add(1).ok_or_else(overloaded)?;
        let generation = lease
            .render_generation
            .checked_add(1)
            .ok_or_else(overloaded)?;
        lease.route.view_revision = revision;
        lease.render_generation = generation;
        lease.previous = None;
        lease.in_flight = None;
        lease.frozen = false;
        terminal.dirty = true;
        Ok(lease.route)
    }

    fn activate(&mut self, route: InputRoute, viewport: TuiViewport) -> Reply<()> {
        let lease = self
            .lease
            .as_mut()
            .filter(|lease| !lease.armed && lease.route == route)
            .ok_or_else(stale_route)?;
        let terminal = self
            .terminal
            .as_mut()
            .filter(|terminal| terminal.exit.is_none())
            .ok_or_else(terminal_fault)?;
        terminal
            .resize(viewport.content())
            .map_err(|_| terminal_fault())?;
        lease.viewport = viewport;
        terminal.dirty = true;
        lease.armed = true;
        Ok(())
    }

    fn apply(&mut self, client: Uuid, request_id: Option<String>, action: Action) -> Reply<()> {
        let lease = self
            .lease
            .as_mut()
            .filter(|lease| lease.armed && lease.route.client_id == client)
            .ok_or_else(stale_route)?;
        let terminal = self.terminal.as_mut().ok_or_else(terminal_fault)?;
        if lease.frozen && !matches!(action, Action::FrameApplied { .. } | Action::Ping) {
            return Err(stale_route());
        }
        if lease.switch_pending
            && !matches!(
                action,
                Action::FrameApplied { .. } | Action::Ping | Action::Rejected(_)
            )
        {
            return Ok(());
        }
        match action {
            Action::Input {
                route,
                sequence,
                bytes,
            } => {
                lease.check_input(route, sequence, terminal)?;
                match lease.keys(sequence, bytes, &mut terminal.session)? {
                    None => {}
                    Some(KeyCommand::Detach) => {
                        let _ = lease.events.try_send(Event::Detached);
                        self.revoke();
                    }
                    Some(KeyCommand::Tab(command)) => {
                        lease.switch_pending = true;
                        if lease.events.try_send(Event::Tab(command)).is_err() {
                            self.revoke();
                        }
                    }
                    Some(KeyCommand::Close(kind)) => {
                        lease.switch_pending = true;
                        let target = kind.target(lease.location);
                        if lease.events.try_send(Event::Close(target)).is_err() {
                            self.revoke();
                        }
                    }
                    Some(KeyCommand::PickTab) => {
                        lease.picker = self.scope.tab_picker(lease.location);
                    }
                    Some(KeyCommand::PairController) => {
                        if lease.events.try_send(Event::PairController).is_err() {
                            self.revoke();
                        }
                    }
                }
            }
            Action::PasteBegin {
                route,
                sequence,
                id,
            } => {
                lease.check_input(route, sequence, terminal)?;
                if terminal.session.paste_pending() {
                    return Err(input_busy());
                }
                lease.flush_leader(&mut terminal.session)?;
                lease.paste = Some(Paste::new(id, route, request_id));
                lease.input_sequence = sequence;
            }
            Action::PasteChunk { id, bytes } => {
                let mut paste = lease.paste.take().ok_or_else(stale_route)?;
                paste.check(id, lease.route)?;
                paste.append(&bytes)?;
                lease.paste = Some(paste);
            }
            Action::PasteEnd { id } => {
                let paste = lease.paste.take().ok_or_else(stale_route)?;
                paste.check(id, lease.route)?;
                if terminal.exit.is_some() {
                    return Err(stale_route());
                }
                if lease.picker.is_none() && !terminal.session.paste(paste.finish()) {
                    return Err(input_busy());
                }
            }
            Action::Resize {
                connection_generation,
                viewport,
            } => {
                if connection_generation != lease.route.connection_generation {
                    return Err(stale_route());
                }
                if terminal.session.paste_pending() {
                    return Err(input_busy());
                }
                let revision = lease
                    .route
                    .view_revision
                    .checked_add(1)
                    .ok_or_else(overloaded)?;
                let generation = lease
                    .render_generation
                    .checked_add(1)
                    .ok_or_else(overloaded)?;
                let next_viewport = TuiViewport {
                    outer: viewport,
                    ..lease.viewport
                };
                terminal
                    .session
                    .resize_active(next_viewport.content())
                    .map_err(|_| terminal_fault())?;
                lease.viewport = next_viewport;
                lease.leader = None;
                if let Some(paste) = lease.paste.take()
                    && lease
                        .events
                        .try_send(Event::Error {
                            request_id: paste.request_id,
                            fault: stale_route(),
                        })
                        .is_err()
                {
                    self.revoke();
                    return Ok(());
                }
                terminal.dirty = true;
                lease.route.view_revision = revision;
                lease.render_generation = generation;
                lease.previous = None;
                if lease
                    .events
                    .try_send(Event::ViewChanged {
                        route: lease.route,
                        viewport,
                    })
                    .is_err()
                {
                    self.revoke();
                }
            }
            Action::FrameApplied {
                generation,
                frame_id,
            } => lease.ack(generation, frame_id)?,
            Action::Redraw { view_revision } => {
                if view_revision != lease.route.view_revision {
                    return Err(stale_route());
                }
                lease.render_generation = lease
                    .render_generation
                    .checked_add(1)
                    .ok_or_else(overloaded)?;
                lease.previous = None;
                lease.in_flight = None;
                terminal.dirty = true;
            }
            Action::Ping => {
                if lease.events.try_send(Event::Pong { request_id }).is_err() {
                    self.revoke();
                }
            }
            Action::Rejected(fault) => return Err(fault),
        }
        Ok(())
    }

    fn advance(&mut self) {
        self.revoke_if_disconnected();
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        let viewport = self
            .hidden_viewport
            .lock()
            .ok()
            .and_then(|mut pending| pending.take());
        if self.lease.is_none()
            && terminal.exit.is_none()
            && let Some(viewport) = viewport
        {
            let _ = terminal.resize(viewport);
        }
        let failure = match terminal.advance() {
            Ok(failure) => failure,
            Err(_) => {
                self.fail(terminal_fault(), true);
                return;
            }
        };
        if terminal.exit.is_some() {
            self.live.store(false, Ordering::Release);
            if let Some(lease) = &mut self.lease {
                lease.picker = None;
            }
        }
        if terminal.exit.is_some()
            && let Some(failure) = failure
        {
            self.write_failed(failure);
            self.finish_exited();
            return;
        }
        if let Some(lease) = &mut self.lease
            && lease.expire_input(&mut terminal.session).is_err()
        {
            self.fail(overloaded(), false);
            return;
        }
        if let Some(error) = failure {
            self.write_failed(error);
            return;
        }
        if let Some(lease) = &mut self.lease
            && lease.render(terminal, &self.scope.metadata).is_err()
        {
            self.revoke();
        }
        self.finish_exited();
    }

    fn write_failed(&mut self, error: WriteFailure) {
        if error.kind == Kind::Reply {
            self.fail(terminal_fault(), true);
            return;
        }
        let fault = if error.partial {
            ProtocolFault::new(
                "PartialDelivery",
                "input was partially written; inspect the terminal before retrying",
            )
        } else {
            ProtocolFault::new("InputFailed", "PTY input write failed before delivery")
        };
        self.fail(fault, false);
    }

    fn fail(&mut self, fault: ProtocolFault, terminal_failed: bool) {
        if let Some(lease) = &self.lease {
            let _ = lease.events.try_send(Event::Error {
                request_id: None,
                fault,
            });
        }
        self.revoke();
        if terminal_failed {
            self.terminal = None;
            self.live.store(false, Ordering::Release);
        }
    }

    fn finish_exited(&mut self) {
        let Some(terminal) = &self.terminal else {
            return;
        };
        let Some(status) = &terminal.exit else { return };
        if let Some(lease) = &self.lease {
            if terminal.dirty || lease.in_flight.is_some() {
                return;
            }
            let _ = lease.events.try_send(Event::Exited {
                terminal_id: terminal.id,
                status: status.clone(),
            });
        }
        self.lease = None;
        self.viewing.store(false, Ordering::Release);
        self.terminal = None;
    }

    fn revoke_if_disconnected(&mut self) {
        if self.lease.as_ref().is_some_and(|lease| {
            lease.events.is_closed()
                || (!lease.frozen
                    && lease
                        .in_flight
                        .as_ref()
                        .is_some_and(|flight| flight.sent.elapsed() >= COMMAND_TIMEOUT))
        }) {
            self.revoke();
        }
    }

    fn revoke(&mut self) {
        self.lease = None;
        self.viewing.store(false, Ordering::Release);
        if let Some(terminal) = &mut self.terminal
            && terminal.fence_input().is_err()
        {
            self.terminal = None;
            self.live.store(false, Ordering::Release);
        }
    }
}

struct Running {
    id: Uuid,
    registry: Weak<Mutex<Registry>>,
    session: Backend,
    shell: bool,
    dirty: bool,
    exit: Option<ExitStatus>,
    #[cfg(target_os = "linux")]
    process: TerminalProcess,
    #[cfg(windows)]
    provider: Option<Provider>,
}

#[cfg(target_os = "linux")]
struct TerminalProcess {
    managed: Option<ManagedLaunch>,
    probe: Option<Probe>,
}

impl Running {
    fn fence_input(&mut self) -> Result<()> {
        self.session.fence()
    }

    fn spawn(launch: Launch, viewport: Viewport, scope: &LaunchScope) -> Result<Self> {
        let id = Uuid::new_v4();
        let cwd = launch.cwd.clone();
        #[cfg(target_os = "linux")]
        let (session, process, shell, identity) =
            TerminalProcess::spawn(launch, viewport, scope, id)?;
        #[cfg(target_os = "linux")]
        let unmanaged = process.managed.is_none();
        #[cfg(windows)]
        let provider = launch.provider;
        #[cfg(windows)]
        let (session, unmanaged, shell) = {
            let mut command = launch.into_command()?;
            scope.hook_environment.configure(&mut command, None, None);
            (
                Backend::start(scope.host.as_ref(), id, command, viewport)?,
                true,
                false,
            )
        };
        if (unmanaged || cwd.is_some())
            && let Some(registry) = scope.registry.upgrade()
        {
            let mut registry = registry.lock().expect("workspace registry lock poisoned");
            if unmanaged {
                #[cfg(windows)]
                if let Some(provider) = provider {
                    registry
                        .agents
                        .started(id, scope.location, provider, ProcessLiveness::Running);
                }
                #[cfg(target_os = "linux")]
                if shell && let Some(root) = identity {
                    registry.shell_started(id, root);
                }
                registry.terminal_started(id, scope.location);
            }
            if let Some(cwd) = cwd {
                registry.record_cwd(id, &cwd, CwdSource::LaunchDirectory);
            }
        }
        Ok(Self {
            id,
            registry: scope.registry.clone(),
            session,
            shell,
            dirty: true,
            exit: None,
            #[cfg(target_os = "linux")]
            process,
            #[cfg(windows)]
            provider,
        })
    }

    fn resize(&mut self, viewport: Viewport) -> Result<()> {
        self.session.resize(viewport)
    }

    fn recover(saved: RetainedTerminal, scope: &LaunchScope) -> Result<Self> {
        let root = &scope.host.as_ref().ok_or_else(terminal_fault)?.root;
        let mut session = Backend::recover(root, saved.id)?;
        #[cfg(target_os = "linux")]
        let (process, identity) =
            TerminalProcess::started(None, &mut session, scope, saved.id, None);
        let registry = scope.registry.upgrade().ok_or_else(terminal_fault)?;
        let mut registry = registry.lock().expect("workspace registry lock poisoned");
        #[cfg(target_os = "linux")]
        if saved.shell
            && let Some(root) = identity
        {
            registry.shell_started(saved.id, root);
        }
        #[cfg(windows)]
        if let Some(provider) = saved.provider {
            let liveness = match session.status() {
                Ok(None) => ProcessLiveness::Running,
                Ok(Some(status)) => exited(&status),
                Err(_) => ProcessLiveness::Unknown,
            };
            registry
                .agents
                .started(saved.id, scope.location, provider, liveness);
        }
        registry.terminal_started(saved.id, scope.location);
        Ok(Self {
            id: saved.id,
            registry: scope.registry.clone(),
            session,
            shell: saved.shell,
            dirty: true,
            exit: None,
            #[cfg(target_os = "linux")]
            process,
            #[cfg(windows)]
            provider: saved.provider,
        })
    }

    fn advance(&mut self) -> Result<Option<WriteFailure>> {
        if self.exit.is_some() {
            return Ok(None);
        }
        let tick = self.session.poll()?;
        self.dirty |= tick.output;
        if let Some(status) = tick.exit {
            #[cfg(target_os = "linux")]
            {
                self.process.probe = None;
                if let Some(managed) = &self.process.managed {
                    managed.observe(exited(&status), None, Source::ChildExit);
                }
            }
            #[cfg(windows)]
            self.observe(exited(&status));
            self.exit = Some(status);
        } else {
            #[cfg(target_os = "linux")]
            if let Some(cwd) = self.process.sample(tick.output)
                && let Some(registry) = self.registry.upgrade()
            {
                registry
                    .lock()
                    .expect("workspace registry lock poisoned")
                    .record_cwd(self.id, &cwd, CwdSource::ProcessDirectory);
            }
        }
        Ok(tick.failure)
    }

    #[cfg(windows)]
    fn observe(&self, liveness: ProcessLiveness) {
        if self.provider.is_some()
            && let Some(registry) = self.registry.upgrade()
        {
            let mut registry = registry.lock().expect("workspace registry lock poisoned");
            registry.agents.observe(self.id, liveness);
            registry.publish(Vec::new());
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.provider.is_some() && self.exit.is_none() && !self.session.is_retained() {
            let status = self.session.stop();
            self.observe(status.as_ref().map_or(ProcessLiveness::Unknown, exited));
        }
        #[cfg(target_os = "linux")]
        if self.exit.is_none()
            && !self.session.is_retained()
            && let Some(managed) = &self.process.managed
        {
            let status = self.session.stop();
            let (liveness, source) = match status.as_ref() {
                Some(status) => (exited(status), Source::ChildExit),
                None => (ProcessLiveness::Unknown, Source::ProcessProbe),
            };
            managed.observe(liveness, None, source);
        }
        if let Some(registry) = self.registry.upgrade() {
            #[cfg(target_os = "linux")]
            let association = self
                .process
                .managed
                .as_ref()
                .map(|managed| managed.association);
            #[cfg(windows)]
            let association = None;
            registry
                .lock()
                .expect("workspace registry lock poisoned")
                .terminal_closed(self.id, association);
        }
    }
}

#[cfg(target_os = "linux")]
impl TerminalProcess {
    fn spawn(
        launch: Launch,
        viewport: Viewport,
        scope: &LaunchScope,
        id: Uuid,
    ) -> Result<(Backend, Self, bool, Option<Incarnation>)> {
        let shell = launch.provider.is_none() && launch.argv.is_empty();
        let program = launch.provider.and_then(provider::installed_program);
        let executable = launch
            .provider
            .zip(program.as_deref())
            .and_then(|(provider, path)| provider::qualify(provider, path));
        let qualified = executable.is_some();
        let reservation = launch
            .provider
            .map(|provider| {
                ManagedLaunch::reserve(&scope.registry, scope.location, id, provider, qualified)
            })
            .transpose()?;
        let association = executable
            .as_ref()
            .and(reservation.as_ref())
            .map(|launch| launch.association);
        let mut command = launch.into_command(program.filter(|_| qualified), qualified);
        scope
            .hook_environment
            .configure(&mut command, association, shell.then_some(id));

        let mut session = Backend::start(scope.host.as_ref(), id, command, viewport)?;
        let (process, identity) = Self::started(reservation, &mut session, scope, id, executable);
        Ok((session, process, shell, identity))
    }

    fn started(
        managed: Option<ManagedLaunch>,
        session: &mut Backend,
        scope: &LaunchScope,
        terminal: Uuid,
        executable: Option<Executable>,
    ) -> (Self, Option<Incarnation>) {
        let (liveness, source) = match session.status() {
            Ok(None) => (ProcessLiveness::Running, Source::ProcessProbe),
            Ok(Some(status)) => (exited(&status), Source::ChildExit),
            Err(_) => (ProcessLiveness::Unknown, Source::ProcessProbe),
        };
        let process = session
            .pid()
            .zip(scope.boot_id)
            .filter(|_| !matches!(liveness, ProcessLiveness::Exited { .. }));
        let identity = process.and_then(|(pid, boot)| process::incarnation(pid, boot));
        let owner = managed
            .as_ref()
            .and(identity)
            .and_then(|incarnation| ProcessImage::capture(incarnation, executable));
        let expected = executable.or(owner.map(|owner| owner.executable));
        let probe = process.map(|(pid, boot)| {
            Probe::new(
                Arc::clone(&scope.probes),
                pid,
                boot,
                terminal,
                identity,
                expected,
            )
        });
        if let Some(managed) = &managed {
            managed.started(liveness, source, identity, owner);
        }
        (Self { managed, probe }, identity)
    }

    fn sample(&mut self, output: bool) -> Option<PathBuf> {
        let (result, cwd) = self.probe.as_mut()?.sample(output)?;
        if let Some(managed) = &self.managed {
            match result {
                ProbeResult::Status(liveness, identity) => {
                    managed.observe(liveness, identity, Source::ProcessProbe);
                }
                ProbeResult::ImageChanged(identity) => managed.image_changed(identity),
            }
        }
        cwd
    }
}

impl Launch {
    #[cfg(target_os = "linux")]
    fn into_command(self, program: Option<PathBuf>, qualified: bool) -> CommandBuilder {
        let qualified_provider = self.provider.filter(|_| qualified);
        let mut argv = self.argv.into_iter();
        let program = program
            .map(PathBuf::into_os_string)
            .or_else(|| {
                self.provider
                    .map(|provider| OsString::from(provider::program(provider)))
            })
            .or_else(|| argv.next());
        let mut command = if let Some(program) = program {
            let mut command = CommandBuilder::new(program);
            if let Some(provider) = qualified_provider {
                command.args(provider::observer_arguments(provider, argv.as_slice()));
            }
            command.args(argv);
            command
        } else {
            CommandBuilder::new_default_prog()
        };
        if let Some(cwd) = self.cwd {
            command.cwd(cwd);
        }
        command.env("TERM", "corvid");
        command.env("COLORTERM", "truecolor");
        command
    }

    #[cfg(windows)]
    fn into_command(self) -> Result<CommandBuilder> {
        let mut argv = self.argv.into_iter();
        let program = match (self.provider, argv.as_slice().is_empty()) {
            (Some(provider), _) => {
                let name = match provider {
                    Provider::Claude => "claude.exe",
                    Provider::Codex => "codex.exe",
                };
                std::env::var_os("PATH")
                    .as_ref()
                    .into_iter()
                    .flat_map(std::env::split_paths)
                    .filter(|directory| directory.is_absolute())
                    .map(|directory| directory.join(name))
                    .find(|candidate| candidate.is_file())
                    .ok_or_else(|| anyhow::anyhow!("{name} is not on the daemon PATH"))?
                    .into_os_string()
            }
            (None, false) => argv.next().expect("first argument"),
            (None, true) => {
                let root = std::env::var_os("SystemRoot")
                    .ok_or_else(|| anyhow::anyhow!("SystemRoot is unavailable"))?;
                PathBuf::from(root)
                    .join("System32/WindowsPowerShell/v1.0/powershell.exe")
                    .into_os_string()
            }
        };
        let mut command = CommandBuilder::new(program);
        if self.provider.is_some() {
            // The child handle must identify the native executable. Do not
            // fall back to a shell script through PATHEXT.
            command.env("PATHEXT", ".EXE");
        }
        command.args(argv);
        if let Some(cwd) = self.cwd {
            command.cwd(cwd);
        }
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        Ok(command)
    }
}

fn exited(status: &ExitStatus) -> ProcessLiveness {
    ProcessLiveness::Exited {
        code: status.signal().is_none().then(|| status.exit_code()),
    }
}

struct Flight {
    generation: u64,
    frame_id: u64,
    sent: Instant,
}

struct Lease {
    route: InputRoute,
    input_sequence: u64,
    events: mpsc::Sender<Event>,
    render_generation: u64,
    previous: Option<LastFrame>,
    frame_id: u64,
    in_flight: Option<Flight>,
    next_frame: Instant,
    paste: Option<Paste>,
    leader: Option<Instant>,
    picker: Option<TabPicker>,
    armed: bool,
    frozen: bool,
    switch_pending: bool,
    keymap: Keymap,
    viewport: TuiViewport,
    location: Location,
    status: Option<Status>,
}

enum LastFrame {
    Terminal(Option<Box<Screen>>),
    Picker,
}

struct Status {
    layout: Arc<Layout>,
    mode: InputMode,
    cols: u16,
    ansi: String,
}

impl Lease {
    fn new(
        route: InputRoute,
        events: mpsc::Sender<Event>,
        armed: bool,
        keymap: Keymap,
        viewport: TuiViewport,
        location: Location,
    ) -> Self {
        Self {
            route,
            input_sequence: 0,
            events,
            render_generation: route.view_revision,
            previous: None,
            frame_id: 0,
            in_flight: None,
            next_frame: Instant::now(),
            paste: None,
            leader: None,
            picker: None,
            armed,
            frozen: false,
            switch_pending: false,
            keymap,
            viewport,
            location,
            status: None,
        }
    }

    fn input_mode(&self) -> InputMode {
        if self.leader.is_some() || self.picker.is_some() {
            InputMode::Command
        } else {
            InputMode::Passthrough
        }
    }

    fn check_input(&self, route: InputRoute, sequence: u64, terminal: &Running) -> Reply<()> {
        if route != self.route || sequence <= self.input_sequence || terminal.exit.is_some() {
            return Err(stale_route());
        }
        if self.paste.is_some() {
            return Err(input_busy());
        }
        Ok(())
    }

    fn keys(
        &mut self,
        sequence: u64,
        mut bytes: Vec<u8>,
        queue: &mut Backend,
    ) -> Reply<Option<KeyCommand>> {
        if let Some(picker) = &mut self.picker {
            self.input_sequence = sequence;
            return Ok(match picker.key(&bytes) {
                PickerAction::Keep => None,
                PickerAction::Cancel => {
                    self.picker = None;
                    None
                }
                PickerAction::Select(tab) => {
                    self.picker = None;
                    Some(KeyCommand::Tab(TabCommand::Select(tab)))
                }
            });
        }

        if self
            .leader
            .is_some_and(|started| started.elapsed() >= LEADER_TIMEOUT)
        {
            self.flush_leader(queue)?;
        }
        if self.leader.take().is_some() {
            if let Some(command) = self.keymap.command(&bytes) {
                if queue.paste_pending() {
                    return Err(input_busy());
                }
                self.input_sequence = sequence;
                return Ok(Some(command));
            }
            if bytes != [KEY_LEADER] {
                bytes.insert(0, KEY_LEADER);
            }
        } else if bytes == [KEY_LEADER] {
            self.leader = Some(Instant::now());
            self.input_sequence = sequence;
            return Ok(None);
        }
        if !queue.keys(bytes) {
            return Err(overloaded());
        }
        self.input_sequence = sequence;
        Ok(None)
    }

    fn flush_leader(&mut self, queue: &mut Backend) -> Reply<()> {
        if self.leader.is_some() {
            if !queue.keys(vec![KEY_LEADER]) {
                return Err(overloaded());
            }
            self.leader = None;
        }
        Ok(())
    }

    fn expire_input(&mut self, queue: &mut Backend) -> Reply<()> {
        if self
            .leader
            .is_some_and(|started| started.elapsed() >= LEADER_TIMEOUT)
        {
            self.flush_leader(queue)?;
        }
        if self.paste.as_ref().is_some_and(Paste::expired)
            && let Some(paste) = self.paste.take()
        {
            self.events
                .try_send(Event::Error {
                    request_id: paste.request_id,
                    fault: ProtocolFault::new("Timeout", "paste expired before completion"),
                })
                .map_err(|_| overloaded())?;
        }
        Ok(())
    }

    fn ack(&mut self, generation: u64, frame_id: u64) -> Reply<()> {
        if self
            .in_flight
            .as_ref()
            .is_some_and(|flight| flight.generation == generation && flight.frame_id == frame_id)
        {
            self.in_flight = None;
            Ok(())
        } else if generation < self.render_generation {
            Ok(())
        } else {
            Err(ProtocolFault::new(
                "InvalidMessage",
                "unexpected render acknowledgement",
            ))
        }
    }

    fn refresh_status(&mut self, metadata: &MetadataUpdates) -> bool {
        if self.viewport.status_row().is_none() {
            return self.status.take().is_some();
        }
        let layout = metadata.layout();
        let cols = self.viewport.outer.cols.get();
        let mode = self.input_mode();
        if self.status.as_ref().is_some_and(|previous| {
            Arc::ptr_eq(&previous.layout, &layout) && previous.mode == mode && previous.cols == cols
        }) {
            return false;
        }
        let ansi = status::line(&layout, self.location, cols, mode, self.keymap);
        let changed = self
            .status
            .as_ref()
            .is_none_or(|previous| previous.ansi != ansi);
        self.status = Some(Status {
            layout,
            mode,
            cols,
            ansi,
        });
        changed
    }

    fn render(&mut self, terminal: &mut Running, metadata: &MetadataUpdates) -> Result<()> {
        let now = Instant::now();
        if !self.armed || self.frozen || self.in_flight.is_some() || now < self.next_frame {
            return Ok(());
        }
        let Some((ansi, painted)) = self.paint(terminal, metadata)? else {
            return Ok(());
        };

        anyhow::ensure!(ansi.len() <= MAX_ANSI, "render payload exceeds frame limit");
        let frame_id = self.frame_id.checked_add(1).ok_or_else(overloaded)?;
        let render = Render {
            route: self.route,
            generation: self.render_generation,
            frame_id,
            base_frame_id: self.previous.as_ref().map(|_| self.frame_id),
            ansi: ansi.into_bytes(),
        };
        self.events
            .try_send(Event::Render(render))
            .map_err(|_| overloaded())?;
        if matches!(painted, LastFrame::Terminal(_)) {
            terminal.dirty = false;
        }
        self.previous = Some(painted);
        self.frame_id = frame_id;
        self.in_flight = Some(Flight {
            generation: self.render_generation,
            frame_id,
            sent: now,
        });
        self.next_frame = now + FRAME_INTERVAL;
        Ok(())
    }

    fn paint(
        &mut self,
        terminal: &mut Running,
        metadata: &MetadataUpdates,
    ) -> Result<Option<(String, LastFrame)>> {
        if let Some(picker) = &mut self.picker {
            if !picker.dirty && matches!(self.previous, Some(LastFrame::Picker)) {
                return Ok(None);
            }
            let ansi = picker.paint(self.viewport.outer)?;
            return Ok(Some((ansi, LastFrame::Picker)));
        }

        let status_changed = self.refresh_status(metadata);
        let previous = match &self.previous {
            Some(LastFrame::Terminal(screen)) => Some(screen),
            _ => None,
        };
        if !terminal.dirty && !status_changed && previous.is_some() {
            return Ok(None);
        }

        let footer = self
            .viewport
            .status_row()
            .zip(self.status.as_ref())
            .filter(|_| status_changed || previous.is_none())
            .map(|(row, status)| (row, status.ansi.as_str()));
        let (ansi, screen) = terminal
            .session
            .render(previous, self.viewport.outer, footer)?;
        Ok(Some((ansi, LastFrame::Terminal(screen))))
    }
}

fn stale_route() -> ProtocolFault {
    ProtocolFault::new("StaleRoute", "input belongs to a retired terminal route")
}

fn terminal_fault() -> ProtocolFault {
    ProtocolFault::new(
        "TerminalUnavailable",
        "terminal I/O failed or the terminal is no longer live",
    )
}

fn input_busy() -> ProtocolFault {
    let mut fault = ProtocolFault::new("InputBusy", "a paste transaction is in progress");
    fault.retryable = true;
    fault
}
