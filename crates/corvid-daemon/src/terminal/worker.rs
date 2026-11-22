use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::executor::{self, Task};
use anyhow::Result;
use corvid_async::{mpsc, oneshot, watch};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use corvid_core::agent::Source;
use corvid_core::agent::{ProcessLiveness, Provider};
use corvid_core::layout::{CwdSource, Layout, Location};
use corvid_core::{InputRoute, KEY_LEADER, Viewport};
use corvid_protocol::wire::WheelEncoding;
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault, RENDER_LIMIT};
use portable_pty::{CommandBuilder, ExitStatus};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use vt100::Screen;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::agents::ManagedLaunch;
use crate::collector::HookEnvironment;
use crate::metadata::MetadataUpdates;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::process::{
    self, Executable, Incarnation, Probe, ProbeBudget, ProbeResult, ProcessImage,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::provider;
use crate::workspaces::WeakRegistry;

use super::backend::Backend;
use super::commands;
use super::composition::{Composition, HeaderMotion};
use super::host::HostConfig;
use super::input::{Kind, WriteFailure};
use super::keys::{InputMode, KeyCommand, Keymap, TabCommand};
use super::navigation::{self, Mouse, Overlay};
use super::paste::Paste;
use super::status::{self, TuiViewport};
use super::tabs::{PickerAction, TabPicker};
use super::title_motion::TitleMotion;

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
    pub(crate) registry: WeakRegistry,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) boot_id: Option<Uuid>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) probes: Arc<ProbeBudget>,
    pub(crate) hook_environment: Arc<HookEnvironment>,
    pub(crate) host: Option<HostConfig>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct RetainedTerminal {
    pub(crate) id: Uuid,
    shell: bool,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent: Option<crate::agents::RetainedAgent>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[serde(default)]
    agent_handoff: bool,
    #[cfg(windows)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<Provider>,
}

impl LaunchScope {
    fn content(&self, viewport: TuiViewport, location: Location, keymap: Keymap) -> Viewport {
        if keymap.workspace_ui {
            Composition::new(viewport.outer, &self.metadata.layout(), location, None)
                .active_size(location)
        } else {
            viewport.content()
        }
    }

    fn tab_picker(&self, location: Location) -> Option<TabPicker> {
        let metadata = self.metadata.clone();
        self.registry
            .request(move |registry| {
                let layout = metadata.display().layout;
                let workspace = layout
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == location.workspace)?;
                TabPicker::new(workspace, location.tab, |tab| {
                    registry.live_tab(&layout, workspace.id, tab).is_some()
                })
            })
            .flatten()
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
    MouseClick {
        route: InputRoute,
        mouse: Mouse,
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
    RenameTab(InputRoute, String),
    ClipboardWrite(InputRoute, String),
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
    Click(Uuid, Mouse),
    Close(corvid_core::layout::CloseTarget),
    PairController,
    DisconnectOthers,
}

pub(crate) struct Render {
    pub(crate) route: InputRoute,
    pub(crate) generation: u64,
    pub(crate) frame_id: u64,
    pub(crate) base_frame_id: Option<u64>,
    pub(crate) wheel_encoding: WheelEncoding,
    pub(crate) alternate_screen: Option<bool>,
    pub(crate) ansi: Vec<u8>,
}

enum Command {
    Restore(Launch, oneshot::Sender<Reply<Uuid>>),
    CancelRestore(Uuid, oneshot::Sender<Reply<()>>),
    Transfer(
        InputRoute,
        AttachSpec,
        oneshot::Sender<Reply<(InputRoute, mpsc::Sender<Event>)>>,
    ),
    Attach(AttachSpec, oneshot::Sender<Reply<InputRoute>>),
    Action {
        client: Uuid,
        request_id: Option<String>,
        action: Action,
    },
    Fence(InputRoute, oneshot::Sender<Reply<bool>>),
    RetireView(InputRoute, oneshot::Sender<Reply<()>>),
    Freeze(InputRoute, oneshot::Sender<Reply<()>>),
    Thaw(InputRoute, oneshot::Sender<Reply<InputRoute>>),
    Activate(InputRoute, TuiViewport, oneshot::Sender<Reply<()>>),
    Resume(InputRoute),
    Preview(oneshot::Sender<Reply<(Viewport, Vec<u8>)>>),
    History(
        InputRoute,
        bool,
        oneshot::Sender<Reply<corvid_protocol::wire::TerminalHistory>>,
    ),
    Retain(bool, oneshot::Sender<Reply<Option<RetainedTerminal>>>),
}

#[derive(Clone)]
pub(crate) struct TerminalHandle {
    commands: mpsc::Sender<Command>,
    live: Arc<AtomicBool>,
    viewing: Arc<AtomicBool>,
    hidden_viewport: watch::Sender<Option<Viewport>>,
}

impl TerminalHandle {
    pub(crate) async fn restore(&self, launch: Launch) -> Reply<Uuid> {
        let (send, receive) = oneshot::channel();
        self.commands
            .try_send(Command::Restore(launch, send))
            .map_err(|_| overloaded())?;
        receive.await.map_err(|_| terminal_fault())?
    }

    pub(crate) async fn cancel_restore(&self, terminal: Uuid) -> Reply<()> {
        let (send, receive) = oneshot::channel();
        self.commands
            .try_send(Command::CancelRestore(terminal, send))
            .map_err(|_| overloaded())?;
        receive.await.map_err(|_| terminal_fault())?
    }

    pub(crate) fn start(scope: LaunchScope) -> (Self, Task) {
        Self::start_worker(scope, None)
    }

    pub(crate) async fn recover(
        scope: LaunchScope,
        saved: RetainedTerminal,
    ) -> Result<(Self, Task)> {
        let (scope, terminal) = executor::blocking(move || {
            let terminal = Running::recover(saved, &scope)?;
            Ok::<_, anyhow::Error>((scope, terminal))
        })
        .await??;
        Ok(Self::start_worker(scope, Some(terminal)))
    }

    fn start_worker(scope: LaunchScope, terminal: Option<Running>) -> (Self, Task) {
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let live = Arc::new(AtomicBool::new(terminal.is_some()));
        let viewing = Arc::new(AtomicBool::new(false));
        let (hidden_viewport, pending_viewport) = watch::channel(None);
        let mut worker = Worker {
            scope,
            terminal,
            lease: None,
            live: Arc::clone(&live),
            viewing: Arc::clone(&viewing),
            hidden_viewport: pending_viewport,
            retaining: false,
        };
        let worker = executor::spawn_worker(move || worker.run(receiver));
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
        self.hidden_viewport.send_replace(Some(viewport));
    }

    pub(crate) async fn attach(&self, spec: AttachSpec) -> Reply<InputRoute> {
        let (send, receive) = oneshot::channel();
        self.commands
            .try_send(Command::Attach(spec, send))
            .map_err(|_| overloaded())?;
        receive.await.map_err(|_| overloaded())?
    }

    pub(crate) async fn transfer(
        &self,
        route: InputRoute,
        spec: AttachSpec,
    ) -> Reply<(InputRoute, mpsc::Sender<Event>)> {
        let (send, receive) = oneshot::channel();
        self.commands
            .try_send(Command::Transfer(route, spec, send))
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
        shell_only: bool,
    ) -> Reply<corvid_protocol::wire::TerminalHistory> {
        let (send, receive) = oneshot::channel();
        self.commands
            .try_send(Command::History(route, shell_only, send))
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

    pub(crate) async fn retire_view(&self, route: InputRoute) -> Reply<()> {
        let (send, receive) = oneshot::channel();
        self.commands
            .try_send(Command::RetireView(route, send))
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
        crate::executor::time::timeout(COMMAND_TIMEOUT, self.commands.send(Command::Resume(route)))
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
        crate::executor::time::timeout(
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
    crate::executor::time::timeout(COMMAND_TIMEOUT, reply)
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
    hidden_viewport: watch::Receiver<Option<Viewport>>,
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
                    Err(mpsc::error::TryRecvError::Closed) => return,
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
            Command::Restore(launch, reply) => {
                if !reply.is_closed() {
                    let result = if self.terminal.is_some() || self.lease.is_some() {
                        Err(ProtocolFault::new(
                            "TerminalExists",
                            "pane already has a terminal",
                        ))
                    } else {
                        Running::spawn(launch, Viewport::new(112, 26).expect("valid restore size"), &self.scope)
                            .map(|terminal| {
                                let id = terminal.id;
                                self.terminal = Some(terminal);
                                self.live.store(true, Ordering::Release);
                                id
                            })
                            .map_err(|_| ProtocolFault::new("TerminalStartFailed", "could not launch the provider; check its installation on the daemon host"))
                    };
                    let _ = reply.send(result);
                }
            }
            Command::CancelRestore(id, reply) => {
                let result = if self.lease.is_some() {
                    Err(input_busy())
                } else {
                    if self
                        .terminal
                        .as_ref()
                        .is_some_and(|terminal| terminal.id == id)
                    {
                        self.terminal = None;
                        self.live.store(false, Ordering::Release);
                    }
                    Ok(())
                };
                let _ = reply.send(result);
            }
            Command::Attach(spec, reply) if !reply.is_closed() => {
                let _ = reply.send(self.attach(spec));
            }
            Command::Attach(_, _) => {}
            Command::Fence(route, reply) => {
                if !reply.is_closed() {
                    let _ = reply.send(self.fence(route));
                }
            }
            Command::RetireView(route, reply) => {
                if self.lease.as_ref().is_some_and(|lease| {
                    lease.route.client_id == route.client_id
                        && lease.route.connection_generation == route.connection_generation
                }) {
                    self.revoke();
                }
                let _ = reply.send(Ok(()));
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
            Command::Transfer(route, spec, reply) => {
                if !reply.is_closed() && !spec.events.is_closed() {
                    let _ = reply.send(self.transfer(route, spec));
                }
            }
            Command::History(route, shell_only, reply) => {
                if !reply.is_closed() {
                    let _ = reply.send(self.history(route, shell_only));
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

    fn history(
        &mut self,
        route: InputRoute,
        shell_only: bool,
    ) -> Reply<corvid_protocol::wire::TerminalHistory> {
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
        terminal
            .session
            .history(shell_only)
            .map_err(|_| terminal_fault())
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
        if previous.is_some_and(|previous| previous != mode) {
            let route = lease.route;
            self.scope
                .registry
                .request(move |registry| registry.record_input_mode(route, mode));
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
            lease.command_help = false;
            lease.naming = None;
        }
        self.retaining = retain;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let agent = {
            let id = terminal.id;
            Uuid::parse_str(&self.scope.hook_environment.identity.epoch)
                .ok()
                .and_then(|epoch| {
                    self.scope
                        .registry
                        .request(move |registry| registry.agents.retain(id, epoch))
                        .flatten()
                })
        };
        Ok(live.then_some(RetainedTerminal {
            id: terminal.id,
            shell: terminal.shell,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            agent,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            agent_handoff: true,
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
        let content = self
            .scope
            .content(spec.viewport, spec.location, spec.keymap);
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

    fn transfer(
        &mut self,
        route: InputRoute,
        spec: AttachSpec,
    ) -> Reply<(InputRoute, mpsc::Sender<Event>)> {
        let lease = self
            .lease
            .as_ref()
            .filter(|lease| lease.route == route && lease.armed && lease.location == spec.location)
            .ok_or_else(stale_route)?;
        let terminal = self
            .terminal
            .as_mut()
            .filter(|terminal| terminal.exit.is_none())
            .ok_or_else(terminal_fault)?;
        if lease.frozen
            || lease.switch_pending
            || lease.paste.is_some()
            || terminal.session.paste_pending()
        {
            return Err(input_busy());
        }
        terminal
            .session
            .resize_active(
                self.scope
                    .content(spec.viewport, spec.location, spec.keymap),
            )
            .map_err(|_| input_busy())?;
        // Keep the retired connection's event stream alive through the registry commit.
        let previous_events = lease.events.clone();
        let route = InputRoute {
            client_id: spec.client_id,
            terminal_id: terminal.id,
            connection_generation: spec.generation,
            view_revision: spec.revision,
            writer_lease_generation: spec.lease_generation,
        };
        let lease = Lease::new(
            route,
            spec.events,
            true,
            spec.keymap,
            spec.viewport,
            spec.location,
        );
        self.lease = Some(lease);
        terminal.dirty = true;
        Ok((route, previous_events))
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
        lease.command_help = false;
        lease.naming = None;
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
            .resize(self.scope.content(viewport, lease.location, lease.keymap))
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
                let display = lease
                    .keymap
                    .workspace_ui
                    .then(|| self.scope.metadata.display());
                match lease.keys(
                    sequence,
                    bytes,
                    &mut terminal.session,
                    display.as_ref().map(|display| display.layout.as_ref()),
                )? {
                    Some(KeyCommand::BeginRename) => {
                        lease.naming = Some(super::rename::NameEditor::new());
                    }
                    Some(KeyCommand::RenameTab(name)) => {
                        if lease
                            .events
                            .try_send(Event::RenameTab(lease.route, name))
                            .is_err()
                        {
                            self.revoke();
                        }
                    }
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
                    Some(KeyCommand::Browse(kind)) => {
                        let display = display.expect("workspace navigation has a layout");
                        lease.overlay = Some(Overlay::open(kind, &display.layout, lease.location));
                    }
                    Some(KeyCommand::End(target)) => {
                        lease.switch_pending = true;
                        if lease.events.try_send(Event::Close(target)).is_err() {
                            self.revoke();
                        }
                    }
                    Some(KeyCommand::Click(pane, mouse)) => {
                        lease.switch_pending = true;
                        if lease.events.try_send(Event::Click(pane, mouse)).is_err() {
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
                    Some(KeyCommand::DisconnectOthers)
                        if lease.events.try_send(Event::DisconnectOthers).is_err() =>
                    {
                        self.revoke();
                    }
                    Some(KeyCommand::DisconnectOthers) => {}
                }
            }
            Action::MouseClick { route, mouse } => {
                if route != lease.route || !lease.keymap.workspace_ui {
                    return Err(stale_route());
                }
                // Refresh the hosted renderer's input mode after the route has changed.
                let size = self
                    .scope
                    .content(lease.viewport, lease.location, lease.keymap);
                terminal
                    .session
                    .render(None, size, None)
                    .map_err(|_| terminal_fault())?;
                let bytes = mouse.encode(terminal.session.rendered_wheel_encoding(), true);
                if !bytes.is_empty() && !terminal.session.keys(bytes) {
                    return Err(overloaded());
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
                if !lease.command_help && lease.naming.is_none() {
                    lease.flush_leader(&mut terminal.session)?;
                }
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
                let bytes = paste.finish();
                if let Some(editor) = &mut lease.naming {
                    editor.append(&String::from_utf8_lossy(&bytes));
                } else if lease.picker.is_none()
                    && lease.overlay.is_none()
                    && !lease.command_help
                    && !terminal.session.paste(bytes)
                {
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
                    .resize_active(
                        self.scope
                            .content(next_viewport, lease.location, lease.keymap),
                    )
                    .map_err(|_| terminal_fault())?;
                lease.viewport = next_viewport;
                if let Some(cache) = &mut lease.composition {
                    cache.visible = None;
                    cache.pending_layout = None;
                }
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
        let viewport = if self.hidden_viewport.has_changed().unwrap_or(false) {
            *self.hidden_viewport.borrow_and_update()
        } else {
            None
        };
        if self.lease.is_none()
            && terminal.exit.is_none()
            && let Some(viewport) = viewport
        {
            let _ = terminal.resize(viewport);
        }
        let clipboard_route = self.lease.as_ref().and_then(Lease::clipboard_route);
        let (failure, clipboard) = match terminal.advance(clipboard_route) {
            Ok(result) => result,
            Err(_) => {
                self.fail(terminal_fault(), true);
                return;
            }
        };
        if terminal.exit.is_some() {
            self.live.store(false, Ordering::Release);
            if let Some(lease) = &mut self.lease {
                lease.picker = None;
                lease.command_help = false;
                lease.naming = None;
                lease.overlay = None;
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
        if let Some(text) = clipboard
            && let Some(lease) = &self.lease
            && let Some(route) = clipboard_route
            && lease.events.is_empty()
        {
            // Keep space for view changes and renders even if the PTY floods copies.
            let _ = lease.events.try_send(Event::ClipboardWrite(route, text));
        }
        if let Some(lease) = &mut self.lease
            && lease.render(terminal, &self.scope).is_err()
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
    registry: WeakRegistry,
    session: Backend,
    shell: bool,
    dirty: bool,
    reported_title: String,
    clipboard_route: Option<InputRoute>,
    exit: Option<ExitStatus>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    process: TerminalProcess,
    #[cfg(windows)]
    provider: Option<Provider>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
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
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let (session, process, shell, identity) =
            TerminalProcess::spawn(launch, viewport, scope, id)?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let unmanaged = process.managed.is_none();
        #[cfg(windows)]
        let provider = launch.provider;
        #[cfg(windows)]
        let (session, unmanaged, shell) = {
            let mut command = launch.into_command()?;
            scope
                .hook_environment
                .configure(&mut command, Some(id), None);
            (
                Backend::start(scope.host.as_ref(), id, command, viewport)?,
                true,
                provider.is_none(),
            )
        };
        if unmanaged || cwd.is_some() {
            let location = scope.location;
            #[cfg(windows)]
            let process_key = if unmanaged {
                session.process_key()
            } else {
                None
            };
            scope.registry.request(move |registry| {
                if unmanaged {
                    #[cfg(windows)]
                    if let Some(provider) = provider {
                        registry
                            .agents
                            .started(id, location, provider, ProcessLiveness::Running);
                        if let Some(root) = process_key {
                            registry.agents.track_managed(id, root);
                        }
                    } else if let Some(root) = process_key {
                        registry.agents.track_shell(id, root);
                    }
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    if shell && let Some(root) = identity {
                        registry.shell_started(id, root);
                    }
                    registry.terminal_started(id, location);
                }
                if let Some(cwd) = cwd {
                    registry.record_cwd(id, &cwd, CwdSource::LaunchDirectory);
                }
            });
        }
        Ok(Self {
            id,
            registry: scope.registry.clone(),
            session,
            shell,
            dirty: true,
            reported_title: String::new(),
            clipboard_route: None,
            exit: None,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
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
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let (process, identity) =
            TerminalProcess::started(None, &mut session, scope, saved.id, None);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let recovered = saved.agent.filter(|agent| {
            agent.valid() && identity.is_some_and(|root| agent.owner.belongs_to(root))
        });
        // v0.2.5 and earlier handed off only PTY identities. On Linux, bind
        // their surviving foreground provider using its original UUID scope,
        // saved conversation, process birth and qualified executable.
        #[cfg(target_os = "linux")]
        let recovered = recovered.or_else(|| {
            if saved.agent_handoff {
                return None;
            }
            crate::agents::RetainedAgent::legacy(
                identity?,
                scope.metadata.retained_session(saved.id)?,
                Uuid::parse_str(&scope.hook_environment.identity.host_id).ok()?,
            )
        });
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let recovered = recovered.map(|mut agent| {
            agent.recover_restart_path();
            agent
        });
        let location = scope.location;
        #[cfg(windows)]
        let liveness = if saved.provider.is_some() {
            match session.status() {
                Ok(None) => ProcessLiveness::Running,
                Ok(Some(status)) => exited(&status),
                Err(_) => ProcessLiveness::Unknown,
            }
        } else {
            ProcessLiveness::Unknown
        };
        #[cfg(windows)]
        let process_key = session.process_key();
        scope
            .registry
            .request(move |registry| {
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                if saved.shell
                    && let Some(root) = identity
                {
                    registry.shell_started(saved.id, root);
                }
                #[cfg(windows)]
                if let Some(provider) = saved.provider {
                    registry
                        .agents
                        .started(saved.id, location, provider, liveness);
                    if let Some(root) = process_key {
                        registry.agents.track_managed(saved.id, root);
                    }
                } else if let Some(root) = process_key {
                    registry.agents.track_shell(saved.id, root);
                }
                registry.terminal_started(saved.id, location);
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                if let Some(agent) = recovered {
                    // Poll the retained owner independently of the shell/PTY process.
                    registry.recover_agent(
                        saved.id,
                        identity.expect("verified root"),
                        location,
                        agent,
                    );
                }
            })
            .ok_or_else(terminal_fault)?;
        Ok(Self {
            id: saved.id,
            registry: scope.registry.clone(),
            session,
            shell: saved.shell,
            dirty: true,
            reported_title: String::new(),
            clipboard_route: None,
            exit: None,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            process,
            #[cfg(windows)]
            provider: saved.provider,
        })
    }

    fn advance(
        &mut self,
        clipboard_route: Option<InputRoute>,
    ) -> Result<(Option<WriteFailure>, Option<String>)> {
        if self.exit.is_some() {
            return Ok((None, None));
        }
        let clipboard_reset = self.clipboard_route != clipboard_route;
        self.clipboard_route = clipboard_route;
        let tick = self
            .session
            .poll(clipboard_route.is_some(), clipboard_reset)?;
        if tick.title != self.reported_title {
            self.reported_title = tick.title;
            let id = self.id;
            let title = self.reported_title.clone();
            self.registry
                .request(move |registry| registry.record_title(id, title));
        }
        self.dirty |= tick.output;
        if let Some(status) = tick.exit {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
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
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            if let Some(cwd) = self.process.sample(tick.output) {
                let id = self.id;
                self.registry.request(move |registry| {
                    registry.record_cwd(id, &cwd, CwdSource::ProcessDirectory)
                });
            }
        }
        Ok((tick.failure, tick.clipboard))
    }

    #[cfg(windows)]
    fn observe(&self, liveness: ProcessLiveness) {
        if self.provider.is_some() {
            let id = self.id;
            self.registry.request(move |registry| {
                registry.agents.observe(id, liveness);
                registry.publish(Vec::new());
            });
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
        #[cfg(any(target_os = "linux", target_os = "macos"))]
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
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let association = self
            .process
            .managed
            .as_ref()
            .map(|managed| managed.association);
        #[cfg(windows)]
        let association = None;
        let id = self.id;
        self.registry
            .request(move |registry| registry.terminal_closed(id, association));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl TerminalProcess {
    fn spawn(
        launch: Launch,
        viewport: Viewport,
        scope: &LaunchScope,
        id: Uuid,
    ) -> Result<(Backend, Self, bool, Option<Incarnation>)> {
        let shell = launch.provider.is_none() && launch.argv.is_empty();
        let launcher = launch.provider.and_then(provider::installed_program);
        let program = launcher
            .as_deref()
            .and_then(|path| path.canonicalize().ok());
        let executable = launch
            .provider
            .zip(program.as_deref())
            .and_then(|(provider, path)| provider::qualify(provider, path));
        let qualified = executable.is_some();
        let restart_path = launcher
            .as_deref()
            .filter(|_| qualified && launch.provider == Some(Provider::Claude))
            .map(Arc::from);
        let reservation = launch
            .provider
            .map(|provider| {
                ManagedLaunch::reserve(
                    &scope.registry,
                    scope.location,
                    id,
                    provider,
                    qualified,
                    restart_path.clone(),
                )
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
        let (mut process, identity) =
            Self::started(reservation, &mut session, scope, id, executable);
        if let Some(probe) = &mut process.probe {
            probe.allow_restart(restart_path);
        }
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
            .and_then(|incarnation| ProcessImage::capture_started(incarnation, executable));
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
    #[cfg(any(target_os = "linux", target_os = "macos"))]
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
        // Bash and other prompt scripts check the TERM name before emitting
        // OSC titles. The matching terminfo entry describes Corvid's VT subset.
        command.env("TERM", "xterm-corvid-256color");
        command.env("COLORTERM", "truecolor");
        command
    }

    #[cfg(windows)]
    fn into_command(self) -> Result<CommandBuilder> {
        let mut argv = self.argv.into_iter();
        let program = match (self.provider, argv.as_slice().is_empty()) {
            (Some(provider), _) => {
                let name = crate::provider::contract(provider).windows.program;
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
    naming: Option<super::rename::NameEditor>,
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
    overlay: Option<Overlay>,
    composition: Option<CompositionCache>,
    navigation_dirty: bool,
    command_help: bool,
    armed: bool,
    frozen: bool,
    switch_pending: bool,
    keymap: Keymap,
    viewport: TuiViewport,
    location: Location,
    status: Option<Status>,
}

enum LastFrame {
    Naming,
    Terminal(Option<Box<Screen>>),
    Picker,
    Commands(bool),
    Composition,
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
            overlay: None,
            composition: keymap.workspace_ui.then(CompositionCache::new),
            navigation_dirty: false,
            command_help: false,
            naming: None,
            armed,
            frozen: false,
            switch_pending: false,
            keymap,
            viewport,
            location,
            status: None,
        }
    }

    fn clipboard_route(&self) -> Option<InputRoute> {
        (self.keymap.clipboard_write && self.armed && !self.frozen && !self.switch_pending)
            .then_some(self.route)
    }

    fn input_mode(&self) -> InputMode {
        if self.leader.is_some()
            || self.picker.is_some()
            || self.overlay.is_some()
            || self.command_help
            || self.naming.is_some()
        {
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
        layout: Option<&Layout>,
    ) -> Reply<Option<KeyCommand>> {
        if let Some(layout) = layout {
            self.navigation_dirty |=
                self.input_mode() == InputMode::Command || bytes == [KEY_LEADER];
            if let Some(overlay) = self.overlay.take() {
                self.input_sequence = sequence;
                let (next, command) = overlay.key(&bytes, layout, self.location, self.keymap);
                self.overlay = next;
                return Ok(command);
            }
            if let Some(mouse) = Mouse::parse(&bytes) {
                self.input_sequence = sequence;
                if self.input_mode() == InputMode::Command {
                    return Ok(None);
                }
                // Resolve clicks against the layout the client acknowledged drawing.
                let Some(view) = self
                    .composition
                    .as_ref()
                    .and_then(|cache| cache.visible.as_ref())
                else {
                    return Ok(None);
                };
                if mouse.y == 0 && view.panes[0].1.y > 0 {
                    let tab = mouse
                        .is_primary()
                        .then(|| {
                            view.tabs
                                .iter()
                                .find(|tab| tab.rect.contains(mouse.x, mouse.y))
                        })
                        .flatten();
                    return Ok(tab.map(|tab| KeyCommand::Tab(TabCommand::Select(tab.id))));
                }
                if let Some((pane, region)) = view
                    .panes
                    .iter()
                    .find(|(_, rect)| rect.contains(mouse.x, mouse.y))
                {
                    let relative = mouse.relative(view.body(*region));
                    if *pane != self.location.pane && mouse.is_primary() {
                        return Ok(Some(
                            relative.map_or(KeyCommand::Tab(TabCommand::Pane(*pane)), |mouse| {
                                KeyCommand::Click(*pane, mouse)
                            }),
                        ));
                    }
                    if *pane == self.location.pane
                        && let Some(relative) = relative
                    {
                        let bytes = relative.encode(queue.rendered_wheel_encoding(), false);
                        if !bytes.is_empty() && !queue.keys(bytes) {
                            return Err(overloaded());
                        }
                    }
                }
                return Ok(None);
            }
            if self
                .leader
                .is_some_and(|started| started.elapsed() < LEADER_TIMEOUT)
            {
                if bytes == b"?" {
                    self.leader = None;
                    self.input_sequence = sequence;
                    return Ok(Some(KeyCommand::Browse(navigation::Kind::Commands)));
                }
                if let Some(direction) = navigation::direction(&bytes) {
                    self.leader = None;
                    self.input_sequence = sequence;
                    return Ok(navigation::neighbor(
                        layout,
                        self.location,
                        self.location.pane,
                        direction,
                    )
                    .map(|pane| KeyCommand::Tab(TabCommand::Pane(pane))));
                }
            }
        }
        if let Some(editor) = &mut self.naming {
            self.input_sequence = sequence;
            return Ok(match editor.key(&bytes) {
                super::rename::Edit::Keep => None,
                super::rename::Edit::Cancel => {
                    self.naming = None;
                    None
                }
                super::rename::Edit::Save(name) => {
                    self.naming = None;
                    Some(KeyCommand::RenameTab(name))
                }
            });
        }
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
        if self.command_help || self.leader.is_some() {
            if matches!(bytes.as_slice(), b"\x1b" | b"\x03") {
                self.leader = None;
                self.command_help = false;
                self.input_sequence = sequence;
                return Ok(None);
            }
            if bytes == b"?" {
                self.leader = None;
                self.command_help = true;
                self.input_sequence = sequence;
                return Ok(None);
            }
        }
        if self.command_help {
            self.input_sequence = sequence;
            if let Some(command) = self.keymap.command(&bytes) {
                if queue.paste_pending() {
                    return Err(input_busy());
                }
                self.command_help = false;
                return Ok(Some(command));
            }
            if bytes == [KEY_LEADER] {
                if !queue.keys(bytes) {
                    return Err(overloaded());
                }
                self.command_help = false;
            }
            return Ok(None);
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
            self.navigation_dirty = true;
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
            if let Some(cache) = &mut self.composition
                && let Some(layout) = cache.pending_layout.take()
            {
                cache.visible = Some(layout);
            }
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
        let display = metadata.display();
        let layout = display.layout;
        let cols = self.viewport.outer.cols.get();
        let mode = self.input_mode();
        if self.status.as_ref().is_some_and(|previous| {
            Arc::ptr_eq(&previous.layout, &layout) && previous.mode == mode && previous.cols == cols
        }) {
            return false;
        }
        let split = layout
            .workspaces
            .iter()
            .flat_map(|workspace| &workspace.tabs)
            .any(|tab| {
                tab.id == self.location.tab
                    && matches!(tab.root, corvid_core::layout::Node::Split { .. })
            });
        let pane = split
            .then(|| layout.pane_title(self.location, &display.titles))
            .flatten();
        let ansi = status::line(&layout, self.location, cols, mode, pane.as_deref());
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

    fn render(&mut self, terminal: &mut Running, scope: &LaunchScope) -> Result<()> {
        let now = Instant::now();
        if !self.armed || self.frozen || self.in_flight.is_some() || now < self.next_frame {
            return Ok(());
        }
        let Some((ansi, painted)) = self.paint(terminal, scope, now)? else {
            return Ok(());
        };

        anyhow::ensure!(ansi.len() <= MAX_ANSI, "render payload exceeds frame limit");
        let frame_id = self.frame_id.checked_add(1).ok_or_else(overloaded)?;
        let render = Render {
            route: self.route,
            generation: self.render_generation,
            frame_id,
            base_frame_id: self.previous.as_ref().map(|_| self.frame_id),
            alternate_screen: if matches!(painted, LastFrame::Terminal(_) | LastFrame::Composition)
            {
                terminal.session.rendered_alternate_screen()
            } else {
                None
            },
            wheel_encoding: if matches!(painted, LastFrame::Terminal(_) | LastFrame::Composition) {
                terminal.session.rendered_wheel_encoding()
            } else {
                WheelEncoding::Disabled
            },
            ansi: ansi.into_bytes(),
        };
        self.events
            .try_send(Event::Render(render))
            .map_err(|_| overloaded())?;
        if matches!(
            painted,
            LastFrame::Terminal(_) | LastFrame::Commands(_) | LastFrame::Composition
        ) {
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

    fn paint_composition(
        &mut self,
        terminal: &mut Running,
        scope: &LaunchScope,
        now: Instant,
    ) -> Result<Option<(String, LastFrame)>> {
        let cache = self.composition.as_mut().expect("composition enabled");
        let display = scope.metadata.display();
        let selected_tab = match self.overlay {
            Some(Overlay::Tabs(id)) => Some(id),
            _ => None,
        };
        let view = Composition::new(
            self.viewport.outer,
            &display.layout,
            self.location,
            selected_tab,
        );
        let size = view.active_size(self.location);
        let layout_changed = cache
            .layout
            .as_ref()
            .is_none_or(|old| !Arc::ptr_eq(old, &display.layout));
        if cache.size != Some(size) && !terminal.session.paste_pending() && self.paste.is_none() {
            terminal.resize(size)?;
            cache.size = Some(size);
            terminal.dirty = true;
        }
        let preview_changed = cache.previews(&view, scope, self.location);
        if let Some(editor) = &self.naming {
            self.navigation_dirty |= editor.dirty;
        }
        let header_changed = view.update_header(&mut cache.header_motion, now);
        let picker = self.overlay.as_ref();
        let picker_target =
            picker.and_then(|overlay| overlay.title_target(&display.layout, self.viewport.outer));
        let picker_changed = cache.picker_motion.update(picker_target, now);
        if !terminal.dirty
            && !layout_changed
            && !preview_changed
            && !self.navigation_dirty
            && self.previous.is_some()
        {
            let mut ansi = if header_changed {
                view.repaint_header(&cache.header_motion)
            } else {
                String::new()
            };
            if picker_changed && let Some(overlay) = picker {
                ansi.push_str(&overlay.paint(
                    &display.layout,
                    self.location,
                    self.keymap,
                    self.viewport.outer,
                    &cache.picker_motion,
                ));
            }
            return Ok((!ansi.is_empty()).then_some((ansi, LastFrame::Composition)));
        }
        if terminal.dirty || cache.active.is_none() {
            let (ansi, snapshot) = terminal.session.render(None, size, None)?;
            cache.active =
                Some(Arc::from(snapshot.unwrap_or_else(|| {
                    Box::new(parse_screen(size, ansi.as_bytes()))
                })));
        }
        let selected = match &self.overlay {
            Some(Overlay::Panes(id)) => Some(*id),
            _ => None,
        };
        let mut ansi = view.paint(
            self.location,
            selected,
            &display.titles,
            &cache.header_motion,
            |pane| {
                if pane == self.location.pane {
                    cache.active.clone()
                } else {
                    cache.screens.get(&pane).cloned()
                }
            },
        );
        if let Some(editor) = &mut self.naming {
            ansi.push_str(&editor.paint(self.viewport.outer));
            editor.dirty = false;
        } else if let Some(overlay) = &self.overlay {
            ansi.push_str(&overlay.paint(
                &display.layout,
                self.location,
                self.keymap,
                self.viewport.outer,
                &cache.picker_motion,
            ));
        } else if self.leader.is_some() {
            ansi.push_str(&navigation::paint_lines(
                self.viewport.outer,
                &["w Workspaces · t Tabs · p Panes · ? Search · Esc Cancel".into()],
            ));
        }
        cache.layout = Some(display.layout);
        cache.pending_layout = Some(view);
        self.navigation_dirty = false;
        Ok(Some((ansi, LastFrame::Composition)))
    }

    fn paint(
        &mut self,
        terminal: &mut Running,
        scope: &LaunchScope,
        now: Instant,
    ) -> Result<Option<(String, LastFrame)>> {
        if self.composition.is_some() {
            return self.paint_composition(terminal, scope, now);
        }
        let metadata = &scope.metadata;
        if let Some(editor) = &mut self.naming {
            if !editor.dirty && matches!(self.previous, Some(LastFrame::Naming)) {
                return Ok(None);
            }
            let (mut ansi, _) = terminal.session.render(None, self.viewport.outer, None)?;
            ansi.push_str(&editor.paint(self.viewport.outer));
            return Ok(Some((ansi, LastFrame::Naming)));
        }
        if let Some(picker) = &mut self.picker {
            let layout = metadata.display().layout;
            if let Some(workspace) = layout
                .workspaces
                .iter()
                .find(|workspace| workspace.id == self.location.workspace)
            {
                picker.refresh_names(workspace);
            }
            if !picker.dirty && matches!(self.previous, Some(LastFrame::Picker)) {
                return Ok(None);
            }
            let ansi = picker.paint(self.viewport.outer)?;
            return Ok(Some((ansi, LastFrame::Picker)));
        }

        let status_changed = self.refresh_status(metadata);
        if self.leader.is_some() || self.command_help {
            if matches!(self.previous, Some(LastFrame::Commands(expanded)) if expanded == self.command_help)
                && !terminal.dirty
                && !status_changed
            {
                return Ok(None);
            }
            let footer = self
                .viewport
                .status_row()
                .zip(self.status.as_ref())
                .map(|(row, status)| (row, status.ansi.as_str()));
            let (mut ansi, _) = terminal.session.render(None, self.viewport.outer, footer)?;
            ansi.push_str(&commands::paint(
                self.viewport.outer,
                self.keymap,
                self.command_help,
            )?);
            return Ok(Some((ansi, LastFrame::Commands(self.command_help))));
        }
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

type PreviewReply = oneshot::Receiver<Reply<(Viewport, Vec<u8>)>>;

struct CompositionCache {
    header_motion: HeaderMotion,
    picker_motion: TitleMotion,
    layout: Option<Arc<Layout>>,
    visible: Option<Composition>,
    pending_layout: Option<Composition>,
    size: Option<Viewport>,
    active: Option<Arc<Screen>>,
    screens: HashMap<Uuid, Arc<Screen>>,
    pending: Option<(Uuid, Instant, PreviewReply)>,
    next_preview: Instant,
    index: usize,
}

impl CompositionCache {
    fn new() -> Self {
        Self {
            header_motion: HeaderMotion::default(),
            picker_motion: TitleMotion::default(),
            layout: None,
            visible: None,
            pending_layout: None,
            size: None,
            active: None,
            screens: HashMap::new(),
            pending: None,
            next_preview: Instant::now(),
            index: 0,
        }
    }

    fn previews(&mut self, view: &Composition, scope: &LaunchScope, location: Location) -> bool {
        let pane_body = |pane| {
            view.panes
                .iter()
                .find(|(id, _)| *id == pane)
                .map(|(_, rect)| view.body(*rect))
        };
        let before = self.screens.len();
        self.screens.retain(|pane, screen| {
            pane_body(*pane).is_some_and(|body| {
                let (rows, cols) = screen.size();
                cols.get() <= body.width && rows.get() <= body.height
            })
        });
        let mut changed = before != self.screens.len();
        if !view.split {
            self.pending = None;
            return changed;
        }
        if let Some((pane, until, mut pending)) = self.pending.take() {
            match pending.try_recv() {
                Ok(Ok((viewport, bytes))) => {
                    let Some(body) = pane_body(pane) else {
                        return changed;
                    };
                    // Each cached preview occupies at most its rendered area.
                    // Reading a wider preview neither resizes nor retains a full copy of that PTY.
                    let cols = viewport.cols.get().min(body.width);
                    let rows = viewport.rows.get().min(body.height);
                    let mut parser = vt100::Parser::new(viewport.rows, viewport.cols, 0);
                    parser.process(&bytes);
                    if (cols, rows) != (viewport.cols.get(), viewport.rows.get()) {
                        let size =
                            Viewport::new(cols, rows).expect("composition has a usable body");
                        parser.set_size(size.rows, size.cols);
                    }
                    let screen = parser.screen();
                    if self
                        .screens
                        .get(&pane)
                        .is_none_or(|previous| !screen.contents_diff(previous).is_empty())
                    {
                        self.screens.insert(pane, Arc::new(screen.clone()));
                        changed = true;
                    }
                }
                Err(oneshot::error::TryRecvError::Empty) if Instant::now() < until => {
                    self.pending = Some((pane, until, pending))
                }
                _ => {
                    changed |= self.screens.remove(&pane).is_some();
                }
            }
        }
        if self.pending.is_none() && Instant::now() >= self.next_preview {
            self.next_preview = Instant::now() + Duration::from_millis(125);
            if let Some(handles) = scope
                .registry
                .request(move |registry| registry.preview_handles(location))
            {
                let before = self.screens.len();
                self.screens
                    .retain(|pane, _| handles.iter().any(|(id, _)| id == pane));
                changed |= before != self.screens.len();
                if !handles.is_empty() {
                    let (pane, handle) = &handles[self.index % handles.len()];
                    self.index = self.index.wrapping_add(1);
                    let (send, reply) = oneshot::channel();
                    if handle.commands.try_send(Command::Preview(send)).is_ok() {
                        self.pending = Some((*pane, Instant::now() + COMMAND_TIMEOUT, reply));
                    }
                }
            }
        }
        changed
    }
}

fn parse_screen(size: Viewport, bytes: &[u8]) -> Screen {
    let mut parser = vt100::Parser::new(size.rows, size.cols, 0);
    parser.process(bytes);
    parser.screen().clone()
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
