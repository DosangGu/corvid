use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use corvid_core::Viewport;
use corvid_protocol::unix::SocketRole;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    Attach, AttachRole, Detach, FrameApplied, InputChunk, LaunchCommand, ResizeViewport,
    RouteToken, Viewport as WireViewport,
};
use corvid_protocol::{
    COMMAND_TIMEOUT, Frame, FrameReader, FrameWriter, PeerRole, ProtocolFault, RenderFrame,
    Session, decode_envelope,
};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval, timeout};
use uuid::Uuid;

use crate::connection::{Connection, open};
use crate::tty::Tty;

pub async fn attach(runtime: &Path, launch: Option<Vec<OsString>>) -> Result<u32> {
    let viewport = Tty::check()?;
    let stream = open(runtime, SocketRole::Client).await?;
    let connection = Connection::establish(
        stream,
        PeerRole::Tui,
        vec!["attach.v1".into(), "render.v1".into()],
    )
    .await?;
    let (connection, route) = timeout(
        COMMAND_TIMEOUT,
        request_attach(connection, viewport, launch),
    )
    .await??;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut interrupt = signal(SignalKind::interrupt())?;

    let tty = Tty::enter()?;
    timeout(COMMAND_TIMEOUT, tty.write_all(b"\x1b[?1049h")).await??;
    let mut client = Interactive::new(connection, viewport, route);
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
) -> Result<(Connection, RouteToken)> {
    let launch = launch
        .map(|argv| -> Result<LaunchCommand> {
            Ok(LaunchCommand {
                argv: argv.into_iter().map(OsString::into_encoded_bytes).collect(),
                cwd: Some(std::env::current_dir()?.as_os_str().as_bytes().to_vec()),
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
    pending_resize: Option<Viewport>,
    input_sequence: u64,
    request_sequence: u64,
    applied: Option<(u64, u64)>,
}

impl Interactive {
    fn new(connection: Connection, viewport: Viewport, route: RouteToken) -> Self {
        Self {
            session: connection.session,
            writer: connection.writer,
            incoming: Incoming::start(connection.reader),
            route,
            viewport,
            pending_resize: None,
            input_sequence: 0,
            request_sequence: 0,
            applied: None,
        }
    }

    async fn run(&mut self, tty: &Tty) -> Result<u32> {
        let mut buffer = [0; 8192];
        let mut resize_tick = interval(Duration::from_millis(50));
        resize_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
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
                    if self.pending_resize.is_none() {
                        self.input(&buffer[..count]).await?;
                    }
                }
                _ = resize_tick.tick() => self.resize().await?,
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
                    Some(Body::Error(error)) => return Err(ProtocolFault::from_wire(&error).into()),
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
            .context("unexpected viewport update")?;
        tty.discard_input()?;
        self.route = route;
        self.applied = None;
        Ok(())
    }

    async fn input(&mut self, data: &[u8]) -> Result<()> {
        self.input_sequence = self
            .input_sequence
            .checked_add(1)
            .context("input sequence exhausted")?;
        self.send(Body::InputChunk(InputChunk {
            route_token: Some(self.route.clone()),
            input_sequence: self.input_sequence,
            data: data.to_vec(),
        }))
        .await
    }

    async fn resize(&mut self) -> Result<()> {
        let Ok(viewport) = Tty::viewport() else {
            return Ok(());
        };
        if self.pending_resize.is_some() || viewport == self.viewport {
            return Ok(());
        }
        self.send(Body::ResizeViewport(ResizeViewport {
            client_id: self.route.client_id.clone(),
            connection_generation: self.route.connection_generation,
            viewport: Some(wire_viewport(viewport)),
        }))
        .await?;
        self.pending_resize = Some(viewport);
        Ok(())
    }

    async fn send(&mut self, body: Body) -> Result<()> {
        self.request_sequence = self
            .request_sequence
            .checked_add(1)
            .context("request sequence exhausted")?;
        let request = self
            .session
            .envelope(Some(self.request_sequence.to_string()), body);
        timeout(COMMAND_TIMEOUT, self.writer.envelope(&request)).await??;
        Ok(())
    }

    async fn detach(&mut self) -> Result<u32> {
        self.send(Body::Detach(Detach {})).await?;
        Ok(0)
    }
}

fn wire_viewport(viewport: Viewport) -> WireViewport {
    WireViewport {
        cols: viewport.cols.get().into(),
        rows: viewport.rows.get().into(),
    }
}
