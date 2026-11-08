use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use corvid_core::agent::Provider;
use corvid_core::layout::Location;
use corvid_core::{InputRoute, Viewport};
use corvid_protocol::controller::PAIR_CAPABILITY;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AgentProvider, Attach, Attached, Envelope, LaunchCommand, MoveTab, Pong, RenderHeader,
    RouteToken, TabMoved, TerminalExited, ViewChanged, Viewport as WireViewport, WriterClaimed,
};
use corvid_protocol::{
    CLAIM_WORKSPACE_CAPABILITY, COMMAND_TIMEOUT, FrameReader, FrameWriter, MOVE_TAB_CAPABILITY,
    PeerRole, ProtocolFault, RenderFrame, Session,
};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::time::timeout;
use uuid::Uuid;

use crate::control::handshake;
use crate::identity::Identity;
use crate::incoming::Incoming;
use crate::projection::selection;
use crate::terminal::{Action, Event, Keymap, Launch, Render};
use crate::workspaces::{
    Active, AttachTarget, Destination, FocusDispatch, FocusRecovery, Workspaces,
};

const CAPABILITIES: &[&str] = &[
    "attach.v1",
    "render.v1",
    "ping.v1",
    "paste.v1",
    "tabs.v1",
    "select_view.v1",
    CLAIM_WORKSPACE_CAPABILITY,
    MOVE_TAB_CAPABILITY,
    PAIR_CAPABILITY,
    "agent_launch.v1",
    corvid_protocol::collector::CODEX_LAUNCH_CAPABILITY,
];
const MAX_INPUT_CHUNK: usize = 8192;

pub(crate) async fn serve(
    stream: UnixStream,
    identity: &Identity,
    workspaces: &Workspaces,
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
    let (focus_tx, focuses) = mpsc::channel(1);
    let target = match session.validate_request(&request) {
        Ok(Body::Attach(attach)) => attach_target(attach),
        Ok(_) => Err(invalid("expected Attach request")),
        Err(error) => Err(error),
    };
    let active = match target {
        Ok(target) => workspaces.attach(client, target, focus_tx).await,
        Err(error) => Err(error),
    };
    let active = match active {
        Ok(active) => active,
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
    let result = serve_active(
        session,
        reader,
        writer,
        request.request_id,
        active,
        workspaces,
        focuses,
    )
    .await;
    workspaces.release(client);
    result
}

async fn serve_active(
    session: Session,
    reader: FrameReader<OwnedReadHalf>,
    mut writer: FrameWriter<OwnedWriteHalf>,
    request_id: Option<String>,
    active: Active,
    workspaces: &Workspaces,
    focuses: mpsc::Receiver<FocusDispatch>,
) -> Result<()> {
    let route = active.route;
    let attached = Attached {
        client_id: route.client_id.to_string(),
        connection_generation: route.connection_generation,
        route_token: Some(wire_route(route)),
        writer_leases: vec![active.location.workspace.to_string()],
        selection: Some(selection(active.location)),
    };
    send(&session, &mut writer, request_id, Body::Attached(attached)).await?;
    TuiConnection {
        session,
        writer,
        active,
        workspaces,
        focuses,
    }
    .run(reader)
    .await
}

struct TuiConnection<'a> {
    session: Session,
    writer: FrameWriter<OwnedWriteHalf>,
    active: Active,
    workspaces: &'a Workspaces,
    focuses: mpsc::Receiver<FocusDispatch>,
}

impl TuiConnection<'_> {
    async fn run(mut self, reader: FrameReader<OwnedReadHalf>) -> Result<()> {
        let mut incoming = Incoming::start(reader);
        loop {
            tokio::select! {
                request = incoming.requests.recv() => {
                    let Some(request) = request else { return Ok(()) };
                    if self.request(request?).await? { return Ok(()) }
                }
                event = self.active.events.recv() => {
                    let Some(event) = event else { return Ok(()) };
                    if self.event(event).await? { return Ok(()) }
                }
                command = self.focuses.recv() => {
                    let Some(command) = command else { return Ok(()) };
                    if self.focus(command).await? { return Ok(()) }
                }
            }
        }
    }

    async fn request(&mut self, request: Envelope) -> Result<bool> {
        let client = self.active.route.client_id;
        let body = match self.session.validate_request(&request) {
            Ok(body) => body,
            Err(fault) => {
                self.fault(request.request_id, fault).await?;
                return Ok(false);
            }
        };
        match body {
            Body::Detach(_) => return Ok(true),
            Body::IssuePairingCode(_) => self.pair(request.request_id).await?,
            Body::SelectView(view) => {
                let destination = exact_destination(view, self.active.route.view_revision);
                match destination {
                    Ok(destination) => self.switch(destination, request.request_id, false).await?,
                    Err(fault) => self.fault(request.request_id, fault).await?,
                }
            }
            Body::ClaimWorkspaceWriter(claim) => {
                let workspace = parse_id(&claim.workspace_id);
                let result = match workspace {
                    Ok(workspace) => self
                        .workspaces
                        .claim(client, workspace, self.active.viewport),
                    Err(fault) => Err(fault),
                };
                match result {
                    Ok(generation) => {
                        let reply = Body::WriterClaimed(WriterClaimed {
                            workspace_id: claim.workspace_id.clone(),
                            generation,
                        });
                        self.send(request.request_id, reply).await?
                    }
                    Err(fault) => self.fault(request.request_id, fault).await?,
                }
            }
            Body::MoveTab(command) => match self.move_tab(command) {
                Ok(moved) => self.send(request.request_id, Body::TabMoved(moved)).await?,
                Err(fault) => self.fault(request.request_id, fault).await?,
            },
            body => {
                let action = command(client, body).unwrap_or_else(Action::Rejected);
                self.active
                    .terminal
                    .action(client, request.request_id, action)
                    .await?;
            }
        }
        Ok(false)
    }

    fn move_tab(&self, command: &MoveTab) -> Result<TabMoved, ProtocolFault> {
        require_view(
            command.expected_view_revision,
            self.active.route.view_revision,
        )?;
        let tab = parse_id(&command.tab_id)?;
        let workspace = parse_id(&command.destination_workspace_id)?;
        self.workspaces.move_tab(&self.active, tab, workspace)
    }

    async fn event(&mut self, event: Event) -> Result<bool> {
        match event {
            Event::Render(render) if render.route == self.active.route => {
                self.render(render).await?
            }
            Event::Render(_) => {}
            Event::ViewChanged { route, viewport } => {
                self.active.route = route;
                self.active.viewport = viewport;
                self.workspaces.resize_hidden(&self.active);
                self.view_changed(None, false).await?;
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
                self.send(None, Body::TerminalExited(exited)).await?;
                return Ok(true);
            }
            Event::Error { request_id, fault } => self.fault(request_id, fault).await?,
            Event::Pong { request_id } => self.send(request_id, Body::Pong(Pong {})).await?,
            Event::Detached => {
                self.send(None, Body::Detach(corvid_protocol::wire::Detach {}))
                    .await?;
                return Ok(true);
            }
            Event::Tab(command) => self.switch(Destination::Tab(command), None, true).await?,
            Event::PairController => self.pair(None).await?,
        }
        Ok(false)
    }

    async fn pair(&mut self, id: Option<String>) -> Result<()> {
        let route = self.active.route;
        match self
            .workspaces
            .issue_pairing_code(route.client_id, route.connection_generation)
        {
            Ok(code) => self.send(id, Body::PairingCode(code)).await,
            Err(fault) => self.fault(id, fault).await,
        }
    }

    async fn focus(&mut self, command: FocusDispatch) -> Result<bool> {
        match self.workspaces.focus(&self.active, &command).await {
            Ok(focused) => {
                let changed = focused.active.is_some();
                if let Some(active) = focused.active {
                    self.active = active;
                }
                command.complete(Ok(focused.reply));
                if changed {
                    self.view_changed(None, false).await?;
                }
            }
            Err(rejected) => {
                command.complete(Err(rejected.fault));
                match rejected.recovery {
                    FocusRecovery::Unchanged => {}
                    FocusRecovery::Restored(route) => {
                        self.active.route = route;
                        self.view_changed(None, false).await?;
                    }
                    FocusRecovery::Lost => return Ok(true),
                }
            }
        }
        Ok(false)
    }

    async fn switch(
        &mut self,
        destination: Destination,
        request_id: Option<String>,
        modal: bool,
    ) -> Result<()> {
        match self.workspaces.switch(&self.active, destination).await {
            Ok(Some(switched)) => {
                self.active = switched.active;
                self.view_changed(request_id, switched.paste_discarded)
                    .await?;
            }
            Ok(None) => {
                if modal {
                    self.active.terminal.resume(self.active.route).await?;
                }
                if request_id.is_some() {
                    self.view_changed(request_id, false).await?;
                }
            }
            Err(fault) => {
                if modal {
                    self.active.terminal.resume(self.active.route).await?;
                }
                self.fault(request_id, fault).await?;
            }
        }
        Ok(())
    }

    async fn view_changed(
        &mut self,
        request_id: Option<String>,
        paste_discarded: bool,
    ) -> Result<()> {
        let viewport = self.active.viewport;
        let view = ViewChanged {
            view_revision: self.active.route.view_revision,
            selection: Some(selection(self.active.location)),
            route_token: Some(wire_route(self.active.route)),
            viewport: Some(WireViewport {
                cols: viewport.cols.get().into(),
                rows: viewport.rows.get().into(),
            }),
            paste_discarded,
        };
        self.send(request_id, Body::ViewChanged(view)).await
    }

    async fn render(&mut self, render: Render) -> Result<()> {
        let header = RenderHeader {
            host_id: self.session.host_id.clone(),
            server_epoch: self.session.server_epoch.clone(),
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
        timeout(COMMAND_TIMEOUT, self.writer.render(&frame)).await??;
        Ok(())
    }

    async fn send(&mut self, id: Option<String>, body: Body) -> Result<()> {
        send(&self.session, &mut self.writer, id, body).await
    }

    async fn fault(&mut self, id: Option<String>, fault: ProtocolFault) -> Result<()> {
        self.send(id, Body::Error(fault.into_wire())).await
    }
}

fn exact_destination(
    view: &corvid_protocol::wire::SelectView,
    revision: u64,
) -> Result<Destination, ProtocolFault> {
    require_view(view.expected_view_revision, revision)?;
    Ok(Destination::Exact(Location {
        workspace: parse_id(&view.workspace_id)?,
        tab: parse_id(&view.tab_id)?,
        pane: parse_id(&view.pane_id)?,
    }))
}

fn require_view(expected: Option<u64>, current: u64) -> Result<(), ProtocolFault> {
    if expected != Some(current) {
        return Err(ProtocolFault::new(
            "StaleRoute",
            "view revision has changed",
        ));
    }
    Ok(())
}

fn parse_id(id: &str) -> Result<Uuid, ProtocolFault> {
    Uuid::parse_str(id).map_err(|_| invalid("invalid layout ID"))
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
        Body::RedrawView(redraw) => Ok(Action::Redraw {
            view_revision: redraw
                .view_revision
                .filter(|revision| *revision > 0)
                .ok_or_else(|| invalid("missing redraw view revision"))?,
        }),
        Body::Ping(_) => Ok(Action::Ping),
        _ => Err(ProtocolFault::new(
            "UnsupportedCapability",
            "unavailable TUI command",
        )),
    }
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

fn attach_target(attach: &Attach) -> Result<AttachTarget, ProtocolFault> {
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

    Ok(AttachTarget {
        workspace: parse_optional_id(attach.workspace_id.as_deref())?,
        tab: parse_optional_id(attach.tab_id.as_deref())?,
        new_workspace: attach.create_workspace.clone(),
        new_tab: attach.create_tab.clone(),
        keymap: Keymap {
            tabs: attach
                .terminal_capabilities
                .iter()
                .any(|name| name == "tabs.v1"),
            pairing: attach
                .terminal_capabilities
                .iter()
                .any(|name| name == PAIR_CAPABILITY),
        },
        viewport: viewport(attach.viewport.as_ref())?,
        launch: attach.launch.as_ref().map(launch).transpose()?,
    })
}

fn parse_optional_id(id: Option<&str>) -> Result<Option<Uuid>, ProtocolFault> {
    id.map(Uuid::parse_str)
        .transpose()
        .map_err(|_| invalid("invalid layout ID"))
}

fn viewport(value: Option<&WireViewport>) -> Result<Viewport, ProtocolFault> {
    let value = value.ok_or_else(|| invalid("missing viewport"))?;
    let error = |_| invalid("unsupported viewport size");
    let cols = u16::try_from(value.cols).map_err(error)?;
    let rows = u16::try_from(value.rows).map_err(error)?;
    Viewport::new(cols, rows).map_err(|_| invalid("unsupported viewport size"))
}

fn launch(command: &LaunchCommand) -> Result<Launch, ProtocolFault> {
    let provider = match command.agent_provider.map(AgentProvider::try_from) {
        None => None,
        Some(Ok(AgentProvider::Claude)) => Some(Provider::Claude),
        Some(Ok(AgentProvider::Codex)) => Some(Provider::Codex),
        _ => return Err(invalid("unsupported managed agent provider")),
    };
    let total: usize = command.argv.iter().map(Vec::len).sum();
    if command.argv.len() > 128
        || total > 64 * 1024
        || (provider.is_none() && command.argv.first().is_some_and(Vec::is_empty))
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
        provider,
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

fn invalid(message: &str) -> ProtocolFault {
    ProtocolFault::new("InvalidMessage", message)
}

fn paste_id(id: &str) -> Result<Uuid, ProtocolFault> {
    Uuid::parse_str(id).map_err(|_| invalid("invalid paste ID"))
}
