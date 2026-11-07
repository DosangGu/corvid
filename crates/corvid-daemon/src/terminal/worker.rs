use std::ffi::OsString;
use std::io::{ErrorKind, Read};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use corvid_core::{InputRoute, KEY_LEADER, Viewport};
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault, RENDER_LIMIT};
use portable_pty::{CommandBuilder, ExitStatus};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;
use vt100::Screen;

use super::input::{InputQueue, Kind, WriteFailure};
use super::paste::Paste;
use super::{TerminalPty, TerminalScreen};

const COMMAND_CAPACITY: usize = 32;
const TICK: Duration = Duration::from_millis(8);
const FRAME_INTERVAL: Duration = Duration::from_millis(34);
const MAX_ANSI: usize = RENDER_LIMIT - 16 * 1024 - 5;
const LEADER_TIMEOUT: Duration = Duration::from_secs(2);

type Reply<T> = Result<T, ProtocolFault>;

pub(crate) struct Launch {
    pub(crate) argv: Vec<OsString>,
    pub(crate) cwd: Option<PathBuf>,
}

pub(crate) struct AttachSpec {
    pub(crate) client_id: Uuid,
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
    ViewChanged(InputRoute),
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
    Disconnect(Uuid),
}

#[derive(Clone)]
pub(crate) struct TerminalHandle {
    commands: mpsc::Sender<Command>,
}

impl TerminalHandle {
    pub(crate) fn start() -> (Self, JoinHandle<()>) {
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let worker = tokio::task::spawn_blocking(|| Worker::default().run(receiver));
        (Self { commands }, worker)
    }

    pub(crate) async fn attach(&self, spec: AttachSpec) -> Reply<InputRoute> {
        let (send, receive) = oneshot::channel();
        self.commands
            .try_send(Command::Attach(spec, send))
            .map_err(|_| overloaded())?;
        receive.await.map_err(|_| overloaded())?
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

    pub(crate) fn disconnect(&self, client: Uuid) {
        // A dropped receiver also revokes the lease on the next worker tick.
        let _ = self.commands.try_send(Command::Disconnect(client));
    }
}

fn overloaded() -> ProtocolFault {
    ProtocolFault::new("LimitExceeded", "terminal command queue is unavailable")
}

#[derive(Default)]
struct Worker {
    terminal: Option<Running>,
    lease: Option<Lease>,
    generation: u64,
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
            Command::Disconnect(client) => {
                if self
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.route.client_id == client)
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
        let generation = self.generation.checked_add(1).ok_or_else(overloaded)?;
        match (&mut self.terminal, spec.launch) {
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
                self.terminal = Some(Running::spawn(launch, spec.viewport).map_err(|_| {
                    ProtocolFault::new(
                        "TerminalStartFailed",
                        "unable to start the requested terminal",
                    )
                })?);
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
            connection_generation: generation,
            view_revision: 1,
            writer_lease_generation: generation,
        };
        self.lease = Some(Lease::new(route, spec.events));
        self.generation = generation;
        Ok(route)
    }

    fn apply(&mut self, client: Uuid, request_id: Option<String>, action: Action) -> Reply<()> {
        let lease = self
            .lease
            .as_mut()
            .filter(|lease| lease.route.client_id == client)
            .ok_or_else(stale_route)?;
        let terminal = self.terminal.as_mut().ok_or_else(terminal_fault)?;
        match action {
            Action::Input {
                route,
                sequence,
                bytes,
            } => {
                lease.check_input(route, sequence, terminal)?;
                if lease.keys(sequence, bytes, &mut terminal.input)? {
                    let _ = lease.events.try_send(Event::Detached);
                    self.revoke();
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
                    .try_send(Event::ViewChanged(lease.route))
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
        let state = match terminal.advance() {
            Ok(state) => state,
            Err(_) => {
                self.fail(terminal_fault(), true);
                return;
            }
        };
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
}

enum ReadState {
    Drained,
    Eof,
    BudgetUsed,
}

impl Running {
    fn spawn(launch: Launch, viewport: Viewport) -> Result<Self> {
        let mut argv = launch.argv.into_iter();
        let mut command = if let Some(program) = argv.next() {
            let mut command = CommandBuilder::new(program);
            command.args(argv);
            command
        } else {
            CommandBuilder::new_default_prog()
        };
        if let Some(cwd) = launch.cwd {
            command.cwd(cwd);
        }
        command.env("TERM", "corvid");
        command.env("COLORTERM", "truecolor");
        Ok(Self {
            id: Uuid::new_v4(),
            pty: TerminalPty::spawn(command, viewport)?,
            screen: TerminalScreen::new(viewport),
            input: InputQueue::default(),
            dirty: true,
            exit: None,
        })
    }

    fn resize(&mut self, viewport: Viewport) -> Result<()> {
        self.pty.resize(viewport)?;
        self.screen.resize(viewport);
        Ok(())
    }

    fn advance(&mut self) -> Result<ReadState> {
        if self.exit.is_some() {
            return Ok(ReadState::Eof);
        }
        let state = self.read_output()?;
        if !matches!(state, ReadState::BudgetUsed)
            && let Some(status) = self.pty.try_wait()?
        {
            self.exit = Some(status);
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
}

impl Lease {
    fn new(route: InputRoute, events: mpsc::Sender<Event>) -> Self {
        Self {
            route,
            input_sequence: 0,
            events,
            render_generation: 1,
            previous: None,
            frame_id: 0,
            in_flight: None,
            next_frame: Instant::now(),
            paste: None,
            leader: None,
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

    fn keys(&mut self, sequence: u64, mut bytes: Vec<u8>, queue: &mut InputQueue) -> Reply<bool> {
        if self
            .leader
            .is_some_and(|started| started.elapsed() >= LEADER_TIMEOUT)
        {
            self.flush_leader(queue)?;
        }
        if self.leader.take().is_some() {
            if bytes == b"d" {
                if queue.paste_pending() {
                    return Err(input_busy());
                }
                return Ok(true);
            }
            if bytes != [KEY_LEADER] {
                bytes.insert(0, KEY_LEADER);
            }
        } else if bytes == [KEY_LEADER] {
            self.leader = Some(Instant::now());
            self.input_sequence = sequence;
            return Ok(false);
        }
        if !queue.user(bytes) {
            return Err(overloaded());
        }
        self.input_sequence = sequence;
        Ok(false)
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
        } else {
            Err(ProtocolFault::new(
                "InvalidMessage",
                "unexpected render acknowledgement",
            ))
        }
    }

    fn render(&mut self, terminal: &mut Running) -> Result<()> {
        let now = Instant::now();
        if !terminal.dirty || self.in_flight.is_some() || now < self.next_frame {
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
