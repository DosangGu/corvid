use std::ffi::OsString;
use std::fmt::Write as _;
use std::future::pending;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use corvid_core::Viewport;
use corvid_protocol::controller::{PAIR_CAPABILITY, PAIRING_SECONDS};
use corvid_protocol::unix::SocketRole;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AgentProvider, Attach, AttachRole, ClaimWorkspaceWriter, Detach, Envelope, FrameApplied,
    InputChunk, LaunchCommand, MoveTab, PairingCode, PasteBegin, PasteChunk, PasteEnd, RedrawView,
    ResizeViewport, RouteToken, ViewChanged, Viewport as WireViewport,
};
use corvid_protocol::{
    CLAIM_WORKSPACE_CAPABILITY, COMMAND_TIMEOUT, Frame, FrameReader, FrameWriter,
    MOVE_TAB_CAPABILITY, PeerRole, ProtocolFault, RenderFrame, Session, decode_envelope,
};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until, timeout};
use uuid::Uuid;

use crate::connection::{UnixConnection as Connection, open};
use crate::keys::{Decoder, Input};
use crate::tty::Tty;

const INPUT_CHUNK: usize = 8192;

pub struct AttachOptions {
    pub launch: Option<Vec<OsString>>,
    pub agent: Option<AgentProvider>,
    pub escape_delay: Duration,
    pub restore_cwd: bool,
    pub workspace: Option<String>,
    pub tab: Option<String>,
    pub new_workspace: Option<String>,
    pub new_tab: Option<String>,
    pub claim_workspaces: Vec<Uuid>,
    /// The tab ID and its destination workspace ID.
    pub move_tab: Option<(Uuid, Uuid)>,
}

impl Default for AttachOptions {
    fn default() -> Self {
        Self {
            launch: None,
            agent: None,
            escape_delay: Duration::from_millis(100),
            restore_cwd: false,
            workspace: None,
            tab: None,
            new_workspace: None,
            new_tab: None,
            claim_workspaces: Vec::new(),
            move_tab: None,
        }
    }
}

pub fn check_tty() -> Result<()> {
    Tty::check().map(|_| ())
}

pub async fn attach(runtime: &Path, mut options: AttachOptions) -> Result<u32> {
    let viewport = Tty::check()?;
    let stream = open(runtime, SocketRole::Client).await?;
    let mut required = vec!["attach.v1".into(), "render.v1".into(), "paste.v1".into()];
    if options.agent.is_some() {
        required.push("agent_launch.v1".into());
    }
    if options.agent == Some(AgentProvider::Codex) {
        required.push(corvid_protocol::collector::CODEX_LAUNCH_CAPABILITY.into());
    }
    let claims = std::mem::take(&mut options.claim_workspaces);
    let move_tab = options.move_tab;
    if !claims.is_empty() {
        required.push(CLAIM_WORKSPACE_CAPABILITY.into());
    }
    if move_tab.is_some() {
        required.push(MOVE_TAB_CAPABILITY.into());
    }
    let connection = Connection::establish(stream, PeerRole::Tui, required).await?;
    let escape_delay = options.escape_delay;
    let (connection, route) = timeout(
        COMMAND_TIMEOUT,
        request_attach(connection, viewport, options),
    )
    .await??;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut interrupt = signal(SignalKind::interrupt())?;

    let tty = Tty::enter()?;
    timeout(COMMAND_TIMEOUT, tty.write_all(b"\x1b[?1049h\x1b[?2004h")).await??;
    let mut client = Interactive::new(connection, viewport, route, escape_delay);
    tokio::select! {
        result = async {
            client.configure_layout(&tty, claims, move_tab).await?;
            client.run(&tty).await
        } => result,
        // A signal can interrupt a partially written frame. Drop the stream so
        // the daemon fences the route on EOF.
        _ = terminate.recv() => Ok(0),
        _ = hangup.recv() => Ok(0),
        _ = interrupt.recv() => Ok(0),
    }
}

async fn request_attach(
    mut connection: Connection,
    viewport: Viewport,
    options: AttachOptions,
) -> Result<(Connection, RouteToken)> {
    let mut terminal_capabilities = vec!["legacy_vt.v1".into(), "tabs.v1".into()];
    if connection
        .session
        .capabilities
        .iter()
        .any(|name| name == PAIR_CAPABILITY)
    {
        terminal_capabilities.push(PAIR_CAPABILITY.into());
    }
    let launch = options
        .launch
        .map(|argv| -> Result<LaunchCommand> {
            Ok(LaunchCommand {
                argv: argv.into_iter().map(OsString::into_encoded_bytes).collect(),
                agent_provider: options.agent.map(|provider| provider as i32),
                cwd: if options.restore_cwd {
                    None
                } else {
                    Some(std::env::current_dir()?.as_os_str().as_bytes().to_vec())
                },
            })
        })
        .transpose()?;
    let request = connection.session.envelope(
        Some(Uuid::new_v4().to_string()),
        Body::Attach(Attach {
            workspace_id: options.workspace,
            tab_id: options.tab,
            create_workspace: options.new_workspace,
            create_tab: options.new_tab,
            viewport: Some(wire_viewport(viewport)),
            terminal_capabilities,
            requested_role: AttachRole::Writer as i32,
            launch,
        }),
    );
    connection.writer.envelope(&request).await?;
    let reply = connection
        .reader
        .next_envelope()
        .await?
        .context("daemon closed during attach")?;
    connection.session.validate_scope(&reply)?;
    ensure!(
        reply.request_id == request.request_id,
        "Attach response ID mismatch"
    );

    let attached = match reply.body {
        Some(Body::Attached(attached)) => attached,
        Some(Body::Error(error)) => return Err(ProtocolFault::from_wire(&error).into()),
        _ => bail!("unexpected Attach response"),
    };
    let route = attached
        .route_token
        .context("attached client has no input route")?;
    ensure!(
        route.client_id == attached.client_id
            && route.connection_generation == attached.connection_generation
            && route.connection_generation > 0
            && route.view_revision > 0
            && route.writer_lease_generation > 0
            && Uuid::parse_str(&route.client_id).is_ok()
            && Uuid::parse_str(&route.terminal_id).is_ok(),
        "invalid initial input route"
    );
    Ok((connection, route))
}

struct Incoming {
    frames: mpsc::Receiver<Result<Frame, corvid_protocol::FrameError>>,
    task: JoinHandle<()>,
}

impl Incoming {
    fn start(mut reader: FrameReader<OwnedReadHalf>) -> Self {
        let (sender, frames) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            loop {
                let item = match reader.next_frame().await {
                    Ok(Some(frame)) => Ok(frame),
                    Ok(None) => return,
                    Err(error) => {
                        let _ = sender.send(Err(error)).await;
                        return;
                    }
                };
                if sender.send(item).await.is_err() {
                    return;
                }
            }
        });
        Self { frames, task }
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Interactive {
    session: Session,
    writer: FrameWriter<OwnedWriteHalf>,
    incoming: Incoming,
    route: RouteToken,
    viewport: Viewport,
    pending_resize: Option<PendingResize>,
    keys: Decoder,
    input_sequence: u64,
    request_sequence: u64,
    applied: Option<(u64, u64)>,
    pairing: Option<PairingDisplay>,
    restoring: bool,
}

struct PendingResize {
    viewport: Viewport,
    request_id: String,
    deadline: Instant,
}

struct PairingDisplay {
    code: Uuid,
    deadline: Instant,
}

impl Interactive {
    fn new(
        connection: Connection,
        viewport: Viewport,
        route: RouteToken,
        escape_delay: Duration,
    ) -> Self {
        Self {
            session: connection.session,
            writer: connection.writer,
            incoming: Incoming::start(connection.reader),
            route,
            viewport,
            pending_resize: None,
            keys: Decoder::new(escape_delay),
            input_sequence: 0,
            request_sequence: 0,
            applied: None,
            pairing: None,
            restoring: false,
        }
    }

    async fn configure_layout(
        &mut self,
        tty: &Tty,
        claims: Vec<Uuid>,
        move_tab: Option<(Uuid, Uuid)>,
    ) -> Result<()> {
        for workspace in claims {
            let workspace_id = workspace.to_string();
            let request = Body::ClaimWorkspaceWriter(ClaimWorkspaceWriter {
                workspace_id: workspace_id.clone(),
            });
            let reply = self.layout_request(tty, request).await?;
            ensure!(
                matches!(reply, Body::WriterClaimed(claim) if claim.workspace_id == workspace_id && claim.generation > 0),
                "unexpected workspace claim reply"
            );
        }

        if let Some((tab, destination)) = move_tab {
            let tab_id = tab.to_string();
            let destination_workspace_id = destination.to_string();
            let request = Body::MoveTab(MoveTab {
                expected_view_revision: Some(self.route.view_revision),
                tab_id: tab_id.clone(),
                destination_workspace_id: destination_workspace_id.clone(),
            });
            let reply = self.layout_request(tty, request).await?;
            let Body::TabMoved(moved) = reply else {
                bail!("unexpected tab move reply");
            };
            ensure!(
                moved.commit_sequence > 0
                    && moved.location.is_some_and(|location| {
                        location.workspace_id == destination_workspace_id
                            && location.tab_id == tab_id
                    }),
                "tab move reply has an unexpected location"
            );
        }
        Ok(())
    }

    async fn layout_request(&mut self, tty: &Tty, request: Body) -> Result<Body> {
        let request_id = self.send(request).await?;
        timeout(COMMAND_TIMEOUT, async {
            loop {
                let frame = self
                    .incoming
                    .frames
                    .recv()
                    .await
                    .context("daemon closed during layout setup")??;
                match frame {
                    Frame::Render(render) => self.render(tty, render).await?,
                    Frame::Structured(payload) => {
                        let envelope = self.scoped_envelope(&payload)?;
                        if envelope.request_id.as_deref() == Some(&request_id) {
                            return match envelope.body.context("missing layout response")? {
                                Body::Error(error) => Err(ProtocolFault::from_wire(&error).into()),
                                body => Ok(body),
                            };
                        }
                        ensure!(
                            self.event(tty, envelope).await?.is_none(),
                            "terminal closed during layout setup"
                        );
                    }
                }
            }
        })
        .await
        .context("layout request timed out")?
    }

    async fn run(&mut self, tty: &Tty) -> Result<u32> {
        let mut buffer = [0; INPUT_CHUNK];
        let mut resize_tick = interval(Duration::from_millis(50));
        resize_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            let input_deadline = self.keys.deadline();
            let pairing_deadline = self.pairing.as_ref().map(|pairing| pairing.deadline);
            tokio::select! {
                item = self.incoming.frames.recv() => {
                    let frame = item.context("daemon closed the attached connection")??;
                    if let Some(status) = self.receive(tty, frame).await? {
                        return Ok(status);
                    }
                }
                count = tty.read(&mut buffer) => {
                    let count = count?;
                    if count == 0 {
                        return self.detach().await;
                    }
                    if self.pairing.is_some() {
                        if buffer[..count].iter().any(|byte| matches!(byte, b'\r' | b'\n' | 0x1b)) {
                            self.dismiss_pairing(tty).await?;
                        }
                        continue;
                    }
                    if self.restoring {
                        continue;
                    }
                    let events = self.keys.feed(&buffer[..count], self.pending_resize.is_none())?;
                    self.forward(events).await?;
                }
                _ = resize_tick.tick() => self.resize().await?,
                _ = wait_until(pairing_deadline) => self.dismiss_pairing(tty).await?,
                _ = wait_until(input_deadline) => {
                    let events = self.keys.expire()?;
                    self.forward(events).await?;
                }
            }
        }
    }

    async fn receive(&mut self, tty: &Tty, frame: Frame) -> Result<Option<u32>> {
        match frame {
            Frame::Render(render) => self.render(tty, render).await?,
            Frame::Structured(payload) => {
                let envelope = self.scoped_envelope(&payload)?;
                return self.event(tty, envelope).await;
            }
        }
        Ok(None)
    }

    fn scoped_envelope(&self, payload: &[u8]) -> Result<Envelope> {
        let envelope = decode_envelope(payload)?;
        self.session.validate_scope(&envelope)?;
        Ok(envelope)
    }

    async fn event(&mut self, tty: &Tty, envelope: Envelope) -> Result<Option<u32>> {
        match envelope.body {
            Some(Body::ViewChanged(view)) => {
                let paste_discarded = view.paste_discarded || self.keys.capturing_paste();
                self.view_changed(tty, view)?;
                if self.pairing.is_some() {
                    self.paint_pairing(tty).await?;
                }
                if paste_discarded {
                    timeout(COMMAND_TIMEOUT, tty.write_all(b"\x07")).await??;
                }
            }
            Some(Body::PairingCode(code)) => self.show_pairing(tty, code).await?,
            Some(Body::TerminalExited(exited)) => {
                ensure!(
                    exited.terminal_id == self.route.terminal_id,
                    "unexpected terminal exit"
                );
                return Ok(Some(exited.exit_code));
            }
            Some(Body::Error(error)) if error.code == "InputBusy" => {
                if self.pending_resize.as_ref().is_some_and(|resize| {
                    envelope.request_id.as_deref() == Some(resize.request_id.as_str())
                }) {
                    self.pending_resize = None;
                } else {
                    timeout(COMMAND_TIMEOUT, tty.write_all(b"\x07")).await??;
                }
            }
            Some(Body::Error(error))
                if matches!(
                    error.code.as_str(),
                    "StaleRoute" | "WorkspaceBusy" | "WriterRequired" | "TargetGone" | "NoTerminal"
                ) =>
            {
                timeout(COMMAND_TIMEOUT, tty.write_all(b"\x07")).await??;
            }
            Some(Body::Error(error)) => return Err(ProtocolFault::from_wire(&error).into()),
            Some(Body::Detach(_)) => return Ok(Some(0)),
            _ => bail!("unsupported daemon event"),
        }
        Ok(None)
    }

    async fn render(&mut self, tty: &Tty, render: RenderFrame) -> Result<()> {
        let header = render.header;
        ensure!(
            header.host_id == self.session.host_id
                && header.server_epoch == self.session.server_epoch
                && header.client_id == self.route.client_id
                && header.view_revision == self.route.view_revision,
            "render frame targets a different connection or view"
        );
        match (self.applied, header.base_frame_id) {
            (None, None) => {}
            (Some((generation, frame_id)), Some(base)) => {
                ensure!(
                    generation == header.render_generation && frame_id == base,
                    "render base mismatch"
                );
            }
            (Some((generation, frame_id)), None) => {
                ensure!(
                    header.render_generation > generation && header.frame_id > frame_id,
                    "stale full render"
                );
            }
            _ => bail!("render stream requires a full redraw"),
        }

        if self.pairing.is_none() && (!self.restoring || header.base_frame_id.is_none()) {
            timeout(COMMAND_TIMEOUT, tty.write_all(&render.ansi)).await??;
            self.restoring = false;
        }
        self.applied = Some((header.render_generation, header.frame_id));
        self.send(Body::FrameApplied(FrameApplied {
            render_generation: header.render_generation,
            frame_id: header.frame_id,
        }))
        .await
        .map(|_| ())
    }

    fn view_changed(&mut self, tty: &Tty, view: ViewChanged) -> Result<()> {
        let route = view.route_token.context("view update has no input route")?;
        ensure!(
            route.view_revision == view.view_revision,
            "inconsistent view revision"
        );
        if route == self.route {
            return Ok(());
        }
        ensure!(
            route.client_id == self.route.client_id
                && route.connection_generation == self.route.connection_generation
                && route.writer_lease_generation > 0
                && Uuid::parse_str(&route.terminal_id).is_ok()
                && self.route.view_revision.checked_add(1) == Some(route.view_revision),
            "view update targets a different or retired route"
        );
        let pending = self.pending_resize.take();
        self.viewport = match view.viewport {
            Some(viewport) => {
                Viewport::new(u16::try_from(viewport.cols)?, u16::try_from(viewport.rows)?)?
            }
            None => pending.context("view update has no viewport")?.viewport,
        };
        self.drain_retired_input(tty)?;
        self.route = route;
        self.applied = None;
        Ok(())
    }

    fn drain_retired_input(&mut self, tty: &Tty) -> Result<()> {
        self.keys.retire();
        let mut bytes = [0; INPUT_CHUNK];
        for _ in 0..8 {
            match tty.try_read(&mut bytes) {
                Ok(0) => return Ok(()),
                Ok(count) => {
                    self.keys.feed(&bytes[..count], false)?;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        bail!("TTY input did not drain during route change")
    }

    async fn forward(&mut self, events: Vec<Input>) -> Result<()> {
        if self.pending_resize.is_some() || self.pairing.is_some() || self.restoring {
            return Ok(());
        }
        for event in events {
            let sequence = self.next_input_sequence()?;
            match event {
                Input::Keys(data) => {
                    self.send(Body::InputChunk(InputChunk {
                        route_token: Some(self.route.clone()),
                        input_sequence: sequence,
                        data,
                    }))
                    .await?;
                }
                Input::Paste(bytes) => self.paste(sequence, &bytes).await?,
            }
        }
        Ok(())
    }

    async fn show_pairing(&mut self, tty: &Tty, code: PairingCode) -> Result<()> {
        ensure!(
            (1..=PAIRING_SECONDS).contains(&code.expires_in_seconds),
            "invalid pairing expiry"
        );
        let code_id = Uuid::parse_str(&code.code)?;
        ensure!(!code_id.is_nil(), "invalid pairing code");
        self.keys.retire();
        self.pairing = Some(PairingDisplay {
            code: code_id,
            deadline: Instant::now() + Duration::from_secs(u64::from(code.expires_in_seconds)),
        });
        self.paint_pairing(tty).await
    }

    async fn paint_pairing(&self, tty: &Tty) -> Result<()> {
        let pairing = self
            .pairing
            .as_ref()
            .context("pairing display is unavailable")?;
        let cols = usize::from(self.viewport.cols.get());
        let rows = usize::from(self.viewport.rows.get());
        let code = pairing.code.to_string();
        let text = if cols * rows >= code.len() {
            format!("{code}\nController pairing code\nEnter or Esc to return")
        } else {
            "Resize to show pairing code".to_owned()
        };
        let mut output = String::from("\x1b[?6l\x1b[r\x1b[0m\x1b[?25l\x1b[2J");
        for (index, line) in text
            .lines()
            .flat_map(|line| line.as_bytes().chunks(cols))
            .take(rows)
            .enumerate()
        {
            write!(
                output,
                "\x1b[{};1H{}",
                index + 1,
                std::str::from_utf8(line)?
            )?;
        }
        timeout(COMMAND_TIMEOUT, tty.write_all(output.as_bytes())).await??;
        Ok(())
    }

    async fn dismiss_pairing(&mut self, tty: &Tty) -> Result<()> {
        self.pairing = None;
        self.restoring = true;
        self.keys.retire();
        tty.discard_input()?;
        self.send(Body::RedrawView(RedrawView {
            view_revision: Some(self.route.view_revision),
        }))
        .await?;
        Ok(())
    }

    fn next_input_sequence(&mut self) -> Result<u64> {
        self.input_sequence = self
            .input_sequence
            .checked_add(1)
            .context("input sequence exhausted")?;
        Ok(self.input_sequence)
    }

    async fn paste(&mut self, sequence: u64, bytes: &[u8]) -> Result<()> {
        let id = Uuid::new_v4().to_string();
        self.send(Body::PasteBegin(PasteBegin {
            route_token: Some(self.route.clone()),
            input_sequence: sequence,
            paste_id: id.clone(),
        }))
        .await?;
        for chunk in bytes.chunks(INPUT_CHUNK) {
            self.send(Body::PasteChunk(PasteChunk {
                paste_id: id.clone(),
                data: chunk.to_vec(),
            }))
            .await?;
        }
        self.send(Body::PasteEnd(PasteEnd { paste_id: id })).await?;
        Ok(())
    }

    async fn resize(&mut self) -> Result<()> {
        if let Some(resize) = &self.pending_resize {
            ensure!(
                Instant::now() < resize.deadline,
                "daemon resize reply timed out"
            );
            return Ok(());
        }
        let Ok(viewport) = Tty::viewport() else {
            return Ok(());
        };
        if viewport == self.viewport || self.keys.capturing_paste() {
            return Ok(());
        }
        self.keys.retire();
        let request_id = self
            .send(Body::ResizeViewport(ResizeViewport {
                client_id: self.route.client_id.clone(),
                connection_generation: self.route.connection_generation,
                viewport: Some(wire_viewport(viewport)),
            }))
            .await?;
        self.pending_resize = Some(PendingResize {
            viewport,
            request_id,
            deadline: Instant::now() + COMMAND_TIMEOUT,
        });
        Ok(())
    }

    async fn send(&mut self, body: Body) -> Result<String> {
        self.request_sequence = self
            .request_sequence
            .checked_add(1)
            .context("request sequence exhausted")?;
        let id = self.request_sequence.to_string();
        let request = self.session.envelope(Some(id.clone()), body);
        timeout(COMMAND_TIMEOUT, self.writer.envelope(&request)).await??;
        Ok(id)
    }

    async fn detach(&mut self) -> Result<u32> {
        self.send(Body::Detach(Detach {})).await?;
        Ok(0)
    }
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => pending().await,
    }
}

fn wire_viewport(viewport: Viewport) -> WireViewport {
    WireViewport {
        cols: viewport.cols.get().into(),
        rows: viewport.rows.get().into(),
    }
}
