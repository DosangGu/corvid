use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use corvid_core::{InputRoute, Viewport};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    Attach, Attached, LaunchCommand, Pong, RenderHeader, RouteToken, TerminalExited, ViewChanged,
    ViewSelection, Viewport as WireViewport,
};
use corvid_protocol::{
    COMMAND_TIMEOUT, FrameReader, FrameWriter, PeerRole, ProtocolFault, RenderFrame, Session,
};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::time::timeout;
use uuid::Uuid;

use crate::control::handshake;
use crate::identity::Identity;
use crate::terminal::{Action, AttachSpec, Event, Launch, TerminalHandle};

const CAPABILITIES: &[&str] = &["attach.v1", "render.v1", "ping.v1", "paste.v1"];
const WORKSPACE: &str = "main";
const MAX_INPUT_CHUNK: usize = 8192;
const EVENTS_CAPACITY: usize = 4;

pub(crate) async fn serve(
    stream: UnixStream,
    identity: &Identity,
    terminal: TerminalHandle,
) -> Result<()> {
    let Some((session, mut reader, mut writer)) = timeout(
        COMMAND_TIMEOUT,
        handshake(stream, identity, PeerRole::Tui, CAPABILITIES),
    )
    .await??
    else {
        return Ok(());
    };
    let request = timeout(COMMAND_TIMEOUT, reader.next_envelope())
        .await??
        .context("missing Attach request")?;
    let client = Uuid::new_v4();
    let (events, receiver) = mpsc::channel(EVENTS_CAPACITY);
    let attach = match session.validate_request(&request) {
        Ok(Body::Attach(attach)) => attach_spec(client, attach, events),
        Ok(_) => Err(invalid("expected Attach request")),
        Err(error) => Err(error),
    };
    let route = match attach {
        Ok(spec) => timeout(COMMAND_TIMEOUT, terminal.attach(spec)).await?,
        Err(error) => Err(error),
    };
    let route = match route {
        Ok(route) => route,
        Err(error) => {
            send(
                &session,
                &mut writer,
                request.request_id,
                Body::Error(error.into_wire()),
            )
            .await?;
            return Ok(());
        }
    };

    let attached = Attached {
        client_id: client.to_string(),
        connection_generation: route.connection_generation,
        route_token: Some(wire_route(route)),
        writer_leases: vec![WORKSPACE.to_owned()],
        selection: Some(selection()),
    };
    send(
        &session,
        &mut writer,
        request.request_id,
        Body::Attached(attached),
    )
    .await?;
    let result = tokio::select! {
        result = read_commands(&session, &terminal, client, &mut reader) => result,
        result = write_events(&session, &mut writer, receiver) => result,
    };
    terminal.disconnect(client);
    result
}

async fn read_commands(
    session: &Session,
    terminal: &TerminalHandle,
    client: Uuid,
    reader: &mut FrameReader<OwnedReadHalf>,
) -> Result<()> {
    while let Some(request) = reader.next_envelope().await? {
        let action = match session.validate_request(&request) {
            Ok(Body::Detach(_)) => return Ok(()),
            Ok(body) => command(client, body).unwrap_or_else(Action::Rejected),
            Err(error) => Action::Rejected(error),
        };
        terminal.action(client, request.request_id, action).await?;
    }
    Ok(())
}

fn command(client: Uuid, body: &Body) -> Result<Action, ProtocolFault> {
    match body {
        Body::InputChunk(input)
            if !input.data.is_empty() && input.data.len() <= MAX_INPUT_CHUNK =>
        {
            let route = input
                .route_token
                .as_ref()
                .ok_or_else(|| invalid("missing input route"))?;
            Ok(Action::Input {
                route: parse_route(route)?,
                sequence: input.input_sequence,
                bytes: input.data.clone(),
            })
        }
        Body::InputChunk(_) => Err(invalid("invalid input chunk size")),
        Body::PasteBegin(paste) => {
            let route = paste
                .route_token
                .as_ref()
                .ok_or_else(|| invalid("missing paste route"))?;
            Ok(Action::PasteBegin {
                route: parse_route(route)?,
                sequence: paste.input_sequence,
                id: paste_id(&paste.paste_id)?,
            })
        }
        Body::PasteChunk(paste)
            if !paste.data.is_empty() && paste.data.len() <= MAX_INPUT_CHUNK =>
        {
            Ok(Action::PasteChunk {
                id: paste_id(&paste.paste_id)?,
                bytes: paste.data.clone(),
            })
        }
        Body::PasteChunk(_) => Err(invalid("invalid paste chunk size")),
        Body::PasteEnd(paste) => Ok(Action::PasteEnd {
            id: paste_id(&paste.paste_id)?,
        }),
        Body::ResizeViewport(resize) if Uuid::parse_str(&resize.client_id) == Ok(client) => {
            Ok(Action::Resize {
                connection_generation: resize.connection_generation,
                viewport: viewport(resize.viewport.as_ref())?,
            })
        }
        Body::ResizeViewport(_) => Err(invalid("resize targets a different client")),
        Body::FrameApplied(frame) => Ok(Action::FrameApplied {
            generation: frame.render_generation,
            frame_id: frame.frame_id,
        }),
        Body::Ping(_) => Ok(Action::Ping),
        _ => Err(ProtocolFault::new(
            "UnsupportedCapability",
            "unavailable TUI command",
        )),
    }
}

async fn write_events(
    session: &Session,
    writer: &mut FrameWriter<OwnedWriteHalf>,
    mut events: mpsc::Receiver<Event>,
) -> Result<()> {
    while let Some(event) = events.recv().await {
        match event {
            Event::Render(render) => {
                let header = RenderHeader {
                    host_id: session.host_id.clone(),
                    server_epoch: session.server_epoch.clone(),
                    client_id: render.route.client_id.to_string(),
                    view_revision: render.route.view_revision,
                    render_generation: render.generation,
                    frame_id: render.frame_id,
                    base_frame_id: render.base_frame_id,
                    ansi_bytes: render.ansi.len() as u32,
                };
                let frame = RenderFrame {
                    header,
                    ansi: render.ansi,
                };
                timeout(COMMAND_TIMEOUT, writer.render(&frame)).await??;
            }
            Event::ViewChanged(route) => {
                let view = ViewChanged {
                    view_revision: route.view_revision,
                    selection: Some(selection()),
                    route_token: Some(wire_route(route)),
                };
                send(session, writer, None, Body::ViewChanged(view)).await?;
            }
            Event::Exited {
                terminal_id,
                status,
            } => {
                let exited = TerminalExited {
                    terminal_id: terminal_id.to_string(),
                    exit_code: status.exit_code(),
                    signal: status.signal().map(str::to_owned),
                };
                send(session, writer, None, Body::TerminalExited(exited)).await?;
                return Ok(());
            }
            Event::Error { request_id, fault } => {
                send(session, writer, request_id, Body::Error(fault.into_wire())).await?;
            }
            Event::Pong { request_id } => {
                send(session, writer, request_id, Body::Pong(Pong {})).await?;
            }
            Event::Detached => {
                send(
                    session,
                    writer,
                    None,
                    Body::Detach(corvid_protocol::wire::Detach {}),
                )
                .await?;
                return Ok(());
            }
        }
    }
    Ok(())
}

async fn send(
    session: &Session,
    writer: &mut FrameWriter<OwnedWriteHalf>,
    request_id: Option<String>,
    body: Body,
) -> Result<()> {
    let envelope = session.envelope(request_id, body);
    timeout(COMMAND_TIMEOUT, writer.envelope(&envelope)).await??;
    Ok(())
}

fn attach_spec(
    client: Uuid,
    attach: &Attach,
    events: mpsc::Sender<Event>,
) -> Result<AttachSpec, ProtocolFault> {
    if attach
        .workspace_id
        .as_deref()
        .is_some_and(|workspace| workspace != WORKSPACE)
    {
        return Err(ProtocolFault::new("TargetGone", "workspace does not exist"));
    }
    if attach.terminal_capabilities.len() > 64
        || !attach
            .terminal_capabilities
            .iter()
            .any(|capability| capability == "legacy_vt.v1")
    {
        return Err(ProtocolFault::new(
            "UnsupportedCapability",
            "TUI requires the legacy VT profile",
        ));
    }

    Ok(AttachSpec {
        client_id: client,
        viewport: viewport(attach.viewport.as_ref())?,
        launch: attach.launch.as_ref().map(launch).transpose()?,
        events,
    })
}

fn viewport(value: Option<&WireViewport>) -> Result<Viewport, ProtocolFault> {
    let value = value.ok_or_else(|| invalid("missing viewport"))?;
    let error = |_| invalid("unsupported viewport size");
    let cols = u16::try_from(value.cols).map_err(error)?;
    let rows = u16::try_from(value.rows).map_err(error)?;
    Viewport::new(cols, rows).map_err(|_| invalid("unsupported viewport size"))
}

fn launch(command: &LaunchCommand) -> Result<Launch, ProtocolFault> {
    let total: usize = command.argv.iter().map(Vec::len).sum();
    if command.argv.len() > 128
        || total > 64 * 1024
        || command.argv.first().is_some_and(Vec::is_empty)
        || command.argv.iter().any(|arg| arg.contains(&0))
    {
        return Err(invalid("invalid launch arguments"));
    }
    let cwd = command
        .cwd
        .as_ref()
        .map(|bytes| PathBuf::from(OsString::from_vec(bytes.clone())));
    if let Some(path) = &cwd
        && (!path.is_absolute()
            || command
                .cwd
                .as_ref()
                .is_some_and(|bytes| bytes.len() > 4096 || bytes.contains(&0)))
    {
        return Err(invalid("invalid launch working directory"));
    }
    Ok(Launch {
        argv: command
            .argv
            .iter()
            .cloned()
            .map(OsString::from_vec)
            .collect(),
        cwd,
    })
}

fn wire_route(route: InputRoute) -> RouteToken {
    RouteToken {
        client_id: route.client_id.to_string(),
        connection_generation: route.connection_generation,
        view_revision: route.view_revision,
        terminal_id: route.terminal_id.to_string(),
        writer_lease_generation: route.writer_lease_generation,
    }
}

fn parse_route(route: &RouteToken) -> Result<InputRoute, ProtocolFault> {
    Ok(InputRoute {
        client_id: Uuid::parse_str(&route.client_id)
            .map_err(|_| invalid("invalid route client ID"))?,
        terminal_id: Uuid::parse_str(&route.terminal_id)
            .map_err(|_| invalid("invalid terminal ID"))?,
        connection_generation: route.connection_generation,
        view_revision: route.view_revision,
        writer_lease_generation: route.writer_lease_generation,
    })
}

fn selection() -> ViewSelection {
    ViewSelection {
        workspace_id: WORKSPACE.to_owned(),
        tab_id: "shell".to_owned(),
        pane_id: "pane".to_owned(),
    }
}

fn invalid(message: &str) -> ProtocolFault {
    ProtocolFault::new("InvalidMessage", message)
}

fn paste_id(id: &str) -> Result<Uuid, ProtocolFault> {
    Uuid::parse_str(id).map_err(|_| invalid("invalid paste ID"))
}
