use std::ffi::OsString;
use std::io::{ErrorKind, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::Result;
use corvid_core::agent::{ProcessLiveness, Provider, Source};
use corvid_core::layout::Location;
use corvid_core::{InputRoute, KEY_LEADER, Viewport};
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault, RENDER_LIMIT};
use portable_pty::{CommandBuilder, ExitStatus};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;
use vt100::Screen;

use crate::agents::ManagedLaunch;
use crate::process::{Probe, ProbeBudget};
use crate::workspaces::Registry;

use super::input::{InputQueue, Kind, WriteFailure};
use super::paste::Paste;
use super::{TerminalPty, TerminalScreen};

const COMMAND_CAPACITY: usize = 32;
const TICK: Duration = Duration::from_millis(8);
const FRAME_INTERVAL: Duration = Duration::from_millis(34);
const MAX_ANSI: usize = RENDER_LIMIT - 16 * 1024 - 5;
const LIVE_SCROLLBACK_ROWS: usize = 128;
const LEADER_TIMEOUT: Duration = Duration::from_secs(2);

type Reply<T> = Result<T, ProtocolFault>;

pub(crate) struct Launch {
    pub(crate) argv: Vec<OsString>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) provider: Option<Provider>,
}

pub(crate) struct LaunchScope {
    pub(crate) location: Location,
    pub(crate) registry: Weak<Mutex<Registry>>,
    pub(crate) boot_id: Option<Uuid>,
    pub(crate) probes: Arc<ProbeBudget>,
}

pub(crate) struct AttachSpec {
    pub(crate) client_id: Uuid,
    pub(crate) generation: u64,
    pub(crate) lease_generation: u64,
    pub(crate) revision: u64,
    pub(crate) dormant: bool,
    pub(crate) tab_keys: bool,
    pub(crate) viewport: Viewport,
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
}

#[derive(Clone, Copy)]
pub(crate) enum TabCommand {
    Next,
    Previous,
    Create,
}

enum KeyEffect {
    Forwarded,
    Detach,
    Tab(TabCommand),
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
    Activate(InputRoute, Viewport, oneshot::Sender<Reply<()>>),
    Resume(InputRoute),
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
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let live = Arc::new(AtomicBool::new(false));
        let viewing = Arc::new(AtomicBool::new(false));
        let hidden_viewport = Arc::new(Mutex::new(None));
        let mut worker = Worker {
            scope,
            terminal: None,
            lease: None,
            live: Arc::clone(&live),
            viewing: Arc::clone(&viewing),
            hidden_viewport: Arc::clone(&hidden_viewport),
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

    pub(crate) async fn fence(&self, route: InputRoute) -> Reply<bool> {
        let (send, receive) = oneshot::channel();
        self.commands
            .try_send(Command::Fence(route, send))
            .map_err(|_| overloaded())?;
        timeout_reply(receive).await
    }

    pub(crate) async fn activate(&self, route: InputRoute, viewport: Viewport) -> Reply<()> {
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
            self.advance();
            if self.terminal.is_some() {
                std::thread::sleep(TICK);
            }
        }
    }

    fn handle(&mut self, command: Command) {
        self.revoke_if_disconnected();
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
    }

    fn attach(&mut self, spec: AttachSpec) -> Reply<InputRoute> {
        if self.lease.is_some() {
            return Err(ProtocolFault::new(
                "WorkspaceBusy",
                "workspace already has a writer",
            ));
        }
        match (&mut self.terminal, spec.launch) {
            (Some(terminal), None) if terminal.exit.is_some() => return Err(terminal_fault()),
            (Some(_), None) if spec.dormant => {}
            (Some(terminal), None) => terminal
                .resize(spec.viewport)
                .map_err(|_| terminal_fault())?,
            (Some(_), Some(_)) => {
                return Err(ProtocolFault::new(
                    "TerminalExists",
                    "attach to the existing terminal",
                ));
            }
            (None, Some(launch)) => {
                self.terminal = Some(Running::spawn(launch, spec.viewport, &self.scope).map_err(
                    |_| {
                        ProtocolFault::new(
                            "TerminalStartFailed",
                            "unable to start the requested terminal",
                        )
                    },
                )?);
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
        self.lease = Some(Lease::new(route, spec.events, !spec.dormant, spec.tab_keys));
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
        if terminal.input.paste_pending() {
            return Err(input_busy());
        }
        let discarded = lease.paste.is_some();
        self.revoke();
        Ok(discarded)
    }

    fn activate(&mut self, route: InputRoute, viewport: Viewport) -> Reply<()> {
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
        terminal.resize(viewport).map_err(|_| terminal_fault())?;
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
                match lease.keys(sequence, bytes, &mut terminal.input)? {
                    KeyEffect::Forwarded => {}
                    KeyEffect::Detach => {
                        let _ = lease.events.try_send(Event::Detached);
                        self.revoke();
                    }
                    KeyEffect::Tab(command) => {
                        lease.switch_pending = true;
                        if lease.events.try_send(Event::Tab(command)).is_err() {
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
                if terminal.input.paste_pending() {
                    return Err(input_busy());
                }
                lease.flush_leader(&mut terminal.input)?;
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
                if !terminal
                    .input
                    .paste(paste.finish(), terminal.screen.bracketed_paste())
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
                if terminal.input.paste_pending() {
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
                terminal.resize(viewport).map_err(|_| terminal_fault())?;
                terminal.input.fence();
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
        let state = match terminal.advance(&self.scope.probes) {
            Ok(state) => state,
            Err(_) => {
                self.fail(terminal_fault(), true);
                return;
            }
        };
        if terminal.exit.is_some() {
            self.live.store(false, Ordering::Release);
        }
        if terminal.exit.is_some()
            && let Some(failure) = terminal.input.interrupted_paste()
        {
            self.write_failed(failure);
            self.finish_exited();
            return;
        }
        if let Some(lease) = &mut self.lease
            && lease.expire_input(&mut terminal.input).is_err()
        {
            self.fail(overloaded(), false);
            return;
        }
        if terminal.exit.is_none()
            && !matches!(state, ReadState::Eof)
            && let Err(error) = terminal.input.flush(&mut terminal.pty)
        {
            self.write_failed(error);
            return;
        }
        if let Some(lease) = &mut self.lease
            && lease.render(terminal).is_err()
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
                || lease
                    .in_flight
                    .as_ref()
                    .is_some_and(|flight| flight.sent.elapsed() >= COMMAND_TIMEOUT)
        }) {
            self.revoke();
        }
    }

    fn revoke(&mut self) {
        self.lease = None;
        self.viewing.store(false, Ordering::Release);
        if let Some(terminal) = &mut self.terminal {
            terminal.input.fence();
        }
    }
}

struct Running {
    id: Uuid,
    pty: TerminalPty,
    screen: TerminalScreen,
    input: InputQueue,
    dirty: bool,
    exit: Option<ExitStatus>,
    managed: Option<ManagedProcess>,
}

struct ManagedProcess {
    launch: ManagedLaunch,
    probe: Option<Probe>,
}

enum ReadState {
    Drained,
    Eof,
    BudgetUsed,
}

impl Running {
    fn spawn(launch: Launch, viewport: Viewport, scope: &LaunchScope) -> Result<Self> {
        let id = Uuid::new_v4();
        let reservation = launch
            .provider
            .map(|provider| ManagedLaunch::reserve(&scope.registry, scope.location, id, provider))
            .transpose()?;
        let mut pty = TerminalPty::spawn(launch.into_command(), viewport)?;
        let managed =
            reservation.map(|launch| ManagedProcess::started(launch, &mut pty, scope.boot_id, id));
        Ok(Self {
            id,
            pty,
            screen: TerminalScreen::with_scrollback(viewport, LIVE_SCROLLBACK_ROWS),
            input: InputQueue::default(),
            dirty: true,
            exit: None,
            managed,
        })
    }

    fn resize(&mut self, viewport: Viewport) -> Result<()> {
        self.pty.resize(viewport)?;
        self.screen.resize(viewport);
        Ok(())
    }

    fn advance(&mut self, budget: &ProbeBudget) -> Result<ReadState> {
        if self.exit.is_some() {
            return Ok(ReadState::Eof);
        }
        let state = self.read_output()?;
        if !matches!(state, ReadState::BudgetUsed)
            && let Some(status) = self.pty.try_wait()?
        {
            if let Some(managed) = &self.managed {
                managed
                    .launch
                    .observe(exited(&status), None, Source::ChildExit);
            }
            self.exit = Some(status);
        } else if let Some(managed) = &mut self.managed
            && let Some((liveness, identity)) = managed
                .probe
                .as_mut()
                .and_then(|probe| probe.sample(budget))
        {
            managed
                .launch
                .observe(liveness, identity, Source::ProcessProbe);
        }
        Ok(state)
    }

    fn read_output(&mut self) -> Result<ReadState> {
        let mut bytes = [0; 8192];
        for _ in 0..16 {
            match self.pty.read(&mut bytes) {
                Ok(0) => return Ok(ReadState::Eof),
                Ok(count) => {
                    self.input.reply(self.screen.process(&bytes[..count])?)?;
                    self.dirty = true;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    return Ok(ReadState::Drained);
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(ReadState::BudgetUsed)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if self.exit.is_none()
            && let Some(managed) = &self.managed
        {
            let status = self.pty.stop();
            let (liveness, source) = match status.as_ref() {
                Some(status) => (exited(status), Source::ChildExit),
                None => (ProcessLiveness::Unknown, Source::ProcessProbe),
            };
            managed.launch.observe(liveness, None, source);
        }
    }
}

impl ManagedProcess {
    fn started(
        launch: ManagedLaunch,
        pty: &mut TerminalPty,
        boot: Option<Uuid>,
        terminal: Uuid,
    ) -> Self {
        let (liveness, source) = match pty.try_wait() {
            Ok(None) => (ProcessLiveness::Running, Source::ProcessProbe),
            Ok(Some(status)) => (exited(&status), Source::ChildExit),
            Err(_) => (ProcessLiveness::Unknown, Source::ProcessProbe),
        };
        let probe = if matches!(liveness, ProcessLiveness::Exited { .. }) {
            None
        } else {
            pty.pid()
                .zip(boot)
                .map(|(pid, boot)| Probe::new(pid, boot, terminal))
        };
        launch.started(liveness, source);
        Self { launch, probe }
    }
}

impl Launch {
    fn into_command(self) -> CommandBuilder {
        let mut argv = self.argv.into_iter();
        let program = match self.provider {
            Some(Provider::Claude) => Some(OsString::from("claude")),
            Some(Provider::Codex) => Some(OsString::from("codex")),
            None => argv.next(),
        };
        let mut command = if let Some(program) = program {
            let mut command = CommandBuilder::new(program);
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
    previous: Option<Screen>,
    frame_id: u64,
    in_flight: Option<Flight>,
    next_frame: Instant,
    paste: Option<Paste>,
    leader: Option<Instant>,
    armed: bool,
    switch_pending: bool,
    tab_keys: bool,
}

impl Lease {
    fn new(route: InputRoute, events: mpsc::Sender<Event>, armed: bool, tab_keys: bool) -> Self {
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
            armed,
            switch_pending: false,
            tab_keys,
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
        queue: &mut InputQueue,
    ) -> Reply<KeyEffect> {
        if self
            .leader
            .is_some_and(|started| started.elapsed() >= LEADER_TIMEOUT)
        {
            self.flush_leader(queue)?;
        }
        if self.leader.take().is_some() {
            let command = match bytes.as_slice() {
                b"d" => Some(KeyEffect::Detach),
                b"n" if self.tab_keys => Some(KeyEffect::Tab(TabCommand::Next)),
                b"p" if self.tab_keys => Some(KeyEffect::Tab(TabCommand::Previous)),
                b"t" if self.tab_keys => Some(KeyEffect::Tab(TabCommand::Create)),
                _ => None,
            };
            if let Some(command) = command {
                if queue.paste_pending() {
                    return Err(input_busy());
                }
                self.input_sequence = sequence;
                return Ok(command);
            }
            if bytes != [KEY_LEADER] {
                bytes.insert(0, KEY_LEADER);
            }
        } else if bytes == [KEY_LEADER] {
            self.leader = Some(Instant::now());
            self.input_sequence = sequence;
            return Ok(KeyEffect::Forwarded);
        }
        if !queue.user(bytes) {
            return Err(overloaded());
        }
        self.input_sequence = sequence;
        Ok(KeyEffect::Forwarded)
    }

    fn flush_leader(&mut self, queue: &mut InputQueue) -> Reply<()> {
        if self.leader.is_some() {
            if !queue.user(vec![KEY_LEADER]) {
                return Err(overloaded());
            }
            self.leader = None;
        }
        Ok(())
    }

    fn expire_input(&mut self, queue: &mut InputQueue) -> Reply<()> {
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

    fn render(&mut self, terminal: &mut Running) -> Result<()> {
        let now = Instant::now();
        if !self.armed || !terminal.dirty || self.in_flight.is_some() || now < self.next_frame {
            return Ok(());
        }

        let ansi = terminal.screen.render(self.previous.as_ref())?.into_bytes();
        anyhow::ensure!(ansi.len() <= MAX_ANSI, "render payload exceeds frame limit");
        let frame_id = self.frame_id.checked_add(1).ok_or_else(overloaded)?;
        let render = Render {
            route: self.route,
            generation: self.render_generation,
            frame_id,
            base_frame_id: self.previous.as_ref().map(|_| self.frame_id),
            ansi,
        };
        self.events
            .try_send(Event::Render(render))
            .map_err(|_| overloaded())?;
        self.previous = Some(terminal.screen.snapshot());
        self.frame_id = frame_id;
        self.in_flight = Some(Flight {
            generation: self.render_generation,
            frame_id,
            sent: now,
        });
        self.next_frame = now + FRAME_INTERVAL;
        terminal.dirty = false;
        Ok(())
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
