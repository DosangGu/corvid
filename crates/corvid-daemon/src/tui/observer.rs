use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_core::Viewport;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    Attached, Pong, RenderHeader, RouteToken, ViewChanged, Viewport as WireViewport,
};
use corvid_protocol::{
    COMMAND_TIMEOUT, FrameReader, FrameWriter, ProtocolFault, RenderFrame, Session,
};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval, timeout};
use uuid::Uuid;

use crate::platform::{ReadHalf, WriteHalf};
use crate::projection::selection;
use crate::terminal::{PROMOTED_RENDER_GENERATION, TerminalScreen, TuiViewport};
use crate::workspaces::{AttachTarget, Workspaces};

use super::{ClientEvents, send, serve_writer, viewport, wire_route};

const INTERVAL: Duration = Duration::from_millis(200);
const VIEW_REVISION: u64 = 1;

struct Registration<'a>(&'a Workspaces, Uuid);

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        self.0.release(self.1);
    }
}

pub(super) async fn serve(
    session: Session,
    mut reader: FrameReader<ReadHalf>,
    mut writer: FrameWriter<WriteHalf>,
    workspaces: &Workspaces,
    request_id: Option<String>,
    target: Result<AttachTarget, ProtocolFault>,
) -> Result<()> {
    let id = Uuid::new_v4();
    let (focus_tx, focuses) = mpsc::channel(1);
    let (mut observer, mut size, accepts_writer) = match target.and_then(|target| {
        let observer = workspaces.observe(id, &target, focus_tx)?;
        Ok((observer, target.viewport, target.keymap.handoff))
    }) {
        Ok(target) => target,
        Err(fault) => {
            return send(
                &session,
                &mut writer,
                request_id,
                Body::Error(fault.into_wire()),
            )
            .await;
        }
    };
    let _registration = Registration(workspaces, id);
    let client = id.to_string();
    let location = observer.location;
    let terminal_id = observer.terminal_id;
    let route = RouteToken {
        client_id: client.clone(),
        connection_generation: observer.generation,
        view_revision: VIEW_REVISION,
        terminal_id: terminal_id.to_string(),
        writer_lease_generation: 0,
    };
    send(
        &session,
        &mut writer,
        request_id,
        Body::Attached(Attached {
            client_id: client.clone(),
            connection_generation: observer.generation,
            route_token: Some(route),
            writer_leases: Vec::new(),
            selection: Some(selection(location)),
        }),
    )
    .await?;

    let mut refresh = interval(INTERVAL);
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut next_id = 1_u64;
    let mut in_flight: Option<(u64, Instant)> = None;
    let mut previous = Vec::new();
    loop {
        tokio::select! {
            biased;
            promoted = &mut observer.promoted => {
                let active = promoted.context("writer promotion was cancelled")?;
                let outer = active.viewport.outer;
                send(&session, &mut writer, None, Body::ViewChanged(ViewChanged {
                    view_revision: active.route.view_revision,
                    selection: Some(selection(active.location)),
                    route_token: Some(wire_route(active.route)),
                    viewport: Some(WireViewport { rows: outer.rows.get().into(), cols: outer.cols.get().into() }),
                    paste_discarded: false,
                })).await?;
                let (_, notices) = mpsc::channel(1);
                return serve_writer(session, reader, writer, active, workspaces, ClientEvents { focuses, notices }).await;
            }
            request = reader.next_envelope() => {
                let Some(request) = request? else { return Ok(()) };
                let reply = match session.validate_request(&request) {
                    Ok(Body::Detach(_)) => return Ok(()),
                    Ok(Body::Ping(_)) => Some(Body::Pong(Pong {})),
                    Ok(Body::FrameApplied(applied)) => {
                        ensure!(in_flight.is_some_and(|(id, _)| id == applied.frame_id && id == applied.render_generation), "unexpected observer frame acknowledgment");
                        in_flight = None;
                        None
                    }
                    Ok(Body::ResizeViewport(resize)) if resize.client_id == client && resize.connection_generation == observer.generation => {
                        match viewport(resize.viewport.as_ref()) {
                            Ok(outer) => { size.outer = outer; workspaces.resize_observer(id, outer); None }
                            Err(fault) => Some(Body::Error(fault.into_wire())),
                        }
                    }
                    Ok(_) => Some(Body::Error(ProtocolFault::new("WriterRequired", "read-only terminal connection").into_wire())),
                    Err(fault) => Some(Body::Error(fault.into_wire())),
                };
                if let Some(reply) = reply {
                    send(&session, &mut writer, request.request_id, reply).await?;
                }
            }
            _ = refresh.tick() => {
                ensure!(in_flight.is_none_or(|(_, at)| at.elapsed() < COMMAND_TIMEOUT), "observer did not acknowledge its frame");
                if !workspaces.can_observe(location, terminal_id) {
                    return send(&session, &mut writer, None, Body::Error(ProtocolFault::new("TargetGone", "observed terminal closed or moved").into_wire())).await;
                }
                if in_flight.is_some() { continue }
                let (canonical, ansi) = observer.terminal.preview().await?;
                if !workspaces.can_observe(location, terminal_id) { continue }
                let ansi = render(canonical, &ansi, size, accepts_writer)?;
                if ansi == previous { continue }
                let id = next_id;
                next_id = id.checked_add(1).filter(|next| *next < PROMOTED_RENDER_GENERATION).context("observer frame ID exhausted")?;
                let frame = RenderFrame {
                    header: RenderHeader {
                        host_id: session.host_id.clone(),
                        server_epoch: session.server_epoch.clone(),
                        client_id: client.clone(),
                        view_revision: VIEW_REVISION,
                        render_generation: id,
                        frame_id: id,
                        base_frame_id: None,
                        ansi_bytes: ansi.len().try_into()?,
                        preview: None,
                    },
                    ansi,
                };
                timeout(COMMAND_TIMEOUT, writer.render(&frame)).await??;
                previous = frame.ansi;
                in_flight = Some((id, Instant::now()));
            }
        }
    }
}

fn render(
    canonical: Viewport,
    ansi: &[u8],
    size: TuiViewport,
    accepts_writer: bool,
) -> Result<Vec<u8>> {
    // Crop the local render model. The worker and PTY keep their canonical size.
    let mut screen = TerminalScreen::new(canonical);
    screen.process(ansi)?;
    let visible = size.content();
    let (row, col) = screen.snapshot().cursor_position();
    let cropped = Viewport {
        rows: canonical.rows.min(visible.rows),
        cols: canonical.cols.min(visible.cols),
    };
    screen.resize(cropped);
    if row >= cropped.rows.get() || col >= cropped.cols.get() {
        screen.process(b"\x1b[?25l")?;
    }
    let label = if accepts_writer {
        "[Read-only] Handoff enabled | Ctrl-] to detach"
    } else {
        "[Read-only] Ctrl-] to detach"
    };
    let label = &label[..label.len().min(usize::from(size.outer.cols.get()))];
    let footer = format!("\x1b[0;30;46m\x1b[2K{label}");
    let footer = (size.status_bar && size.outer.rows.get() > 2)
        .then_some((size.outer.rows.get(), footer.as_str()));
    Ok(screen.render(None, size.outer, footer)?.into_bytes())
}
