use std::ffi::OsString;
use std::future::pending;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use corvid_core::Viewport;
use corvid_protocol::unix::SocketRole;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    Attach, AttachRole, Detach, FrameApplied, InputChunk, LaunchCommand, PasteBegin, PasteChunk,
    PasteEnd, ResizeViewport, RouteToken, Viewport as WireViewport,
};
use corvid_protocol::{
    COMMAND_TIMEOUT, Frame, FrameReader, FrameWriter, PeerRole, ProtocolFault, RenderFrame,
    Session, decode_envelope,
};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until, timeout};
use uuid::Uuid;

use crate::connection::{Connection, open};
use crate::keys::{Decoder, Input};
use crate::tty::Tty;

const INPUT_CHUNK: usize = 8192;

pub async fn attach(
    runtime: &Path,
    launch: Option<Vec<OsString>>,
    escape_delay: Duration,
    restore_cwd: bool,
) -> Result<u32> {
    let viewport = Tty::check()?;
    let stream = open(runtime, SocketRole::Client).await?;
    let connection = Connection::establish(
        stream,
        PeerRole::Tui,
        vec!["attach.v1".into(), "render.v1".into(), "paste.v1".into()],
    )
    .await?;
    let (connection, route) = timeout(
        COMMAND_TIMEOUT,
        request_attach(connection, viewport, launch, restore_cwd),
    )
    .await??;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut interrupt = signal(SignalKind::interrupt())?;

    let tty = Tty::enter()?;
    timeout(COMMAND_TIMEOUT, tty.write_all(b"\x1b[?1049h\x1b[?2004h")).await??;
    let mut client = Interactive::new(connection, viewport, route, escape_delay);
    tokio::select! {
        result = client.run(&tty) => result,
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
    launch: Option<Vec<OsString>>,
    restore_cwd: bool,
) -> Result<(Connection, RouteToken)> {
    let launch = launch
        .map(|argv| -> Result<LaunchCommand> {
            Ok(LaunchCommand {
                argv: argv.into_iter().map(OsString::into_encoded_bytes).collect(),
                cwd: if restore_cwd {
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
            workspace_id: None,
            viewport: Some(wire_viewport(viewport)),
            terminal_capabilities: vec!["legacy_vt.v1".into()],
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
}

struct PendingResize {
    viewport: Viewport,
    request_id: String,
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
        }
    }

    async fn run(&mut self, tty: &Tty) -> Result<u32> {
        let mut buffer = [0; INPUT_CHUNK];
        let mut resize_tick = interval(Duration::from_millis(50));
        resize_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            let input_deadline = self.keys.deadline();
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
                    let events = self.keys.feed(&buffer[..count], self.pending_resize.is_none())?;
                    self.forward(events).await?;
                }
                _ = resize_tick.tick() => self.resize().await?,
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
                let envelope = decode_envelope(&payload)?;
                self.session.validate_scope(&envelope)?;
                match envelope.body {
                    Some(Body::ViewChanged(view)) => {
                        let route = view.route_token.context("view update has no input route")?;
                        ensure!(
                            route.view_revision == view.view_revision,
                            "inconsistent view revision"
                        );
                        self.view_changed(tty, route)?;
                    }
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
                    Some(Body::Error(error)) => return Err(ProtocolFault::from_wire(&error).into()),
                    Some(Body::Detach(_)) => return Ok(Some(0)),
                    _ => bail!("unsupported daemon event"),
                }
            }
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

        timeout(COMMAND_TIMEOUT, tty.write_all(&render.ansi)).await??;
        self.applied = Some((header.render_generation, header.frame_id));
        self.send(Body::FrameApplied(FrameApplied {
            render_generation: header.render_generation,
            frame_id: header.frame_id,
        }))
        .await
        .map(|_| ())
    }

    fn view_changed(&mut self, tty: &Tty, route: RouteToken) -> Result<()> {
        ensure!(
            route.client_id == self.route.client_id
                && route.connection_generation == self.route.connection_generation
                && route.writer_lease_generation == self.route.writer_lease_generation
                && route.terminal_id == self.route.terminal_id
                && self.route.view_revision.checked_add(1) == Some(route.view_revision),
            "view update targets a different or retired route"
        );
        self.viewport = self
            .pending_resize
            .take()
            .context("unexpected viewport update")?
            .viewport;
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
        if self.pending_resize.is_some() {
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
