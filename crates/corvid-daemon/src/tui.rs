use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::platform::{ReadHalf, Stream, WriteHalf, decode_os_string};
use anyhow::Result;
use corvid_core::agent::Provider;
use corvid_core::layout::{Axis, CloseTarget, Location};
use corvid_core::{InputRoute, Viewport};
use corvid_protocol::controller::PAIR_CAPABILITY;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AgentProvider, AgentResponded, Attach, AttachRole, Attached, CloseView, CreateView, Envelope,
    FocusViewAgent, LaunchCommand, MoveTab, PanePreviewFrame, Pong, ReadPanePreview, RenderHeader,
    RouteToken, SplitAxis, TabMoved, TerminalExited, ViewChanged, Viewport as WireViewport,
    WorkspaceClientsDisconnected, WriterClaimed,
};
use corvid_protocol::{
    ADAPTIVE_VIEWPORT_CAPABILITY, CLAIM_WORKSPACE_CAPABILITY, CLOSE_VIEW_CAPABILITY,
    COMMAND_TIMEOUT, CREATE_VIEW_CAPABILITY, DEFAULT_SHELL_CAPABILITY, FrameReader, FrameWriter,
    LEGACY_DEFAULT_LAUNCH_CAPABILITY, MOVE_TAB_CAPABILITY, PANE_PREVIEW_CAPABILITY, PeerRole,
    ProtocolFault, RESPONSE_NOTIFICATION_CAPABILITY, RenderFrame, STATUS_BAR_CAPABILITY, Session,
    TERMINAL_HISTORY_CAPABILITY,
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use uuid::Uuid;

use crate::control::handshake;
use crate::identity::Identity;
use crate::incoming::Incoming;
use crate::projection::selection;
use crate::terminal::{Action, Event, Keymap, Launch, Render, TuiViewport};
use crate::workspaces::{
    Active, AttachTarget, Closed, Destination, FocusDispatch, FocusRecovery, Workspaces,
};

const CAPABILITIES: &[&str] = &[
    corvid_protocol::RESTORE_AGENTS_CAPABILITY,
    corvid_protocol::VIEW_TITLES_CAPABILITY,
    "attach.v1",
    corvid_protocol::PANE_WRITERS_CAPABILITY,
    corvid_protocol::SESSION_CLIENTS_CAPABILITY,
    corvid_protocol::RESUME_ATTACH_CAPABILITY,
    "render.v1",
    "ping.v1",
    "paste.v1",
    "tabs.v1",
    "select_view.v1",
    CLAIM_WORKSPACE_CAPABILITY,
    CREATE_VIEW_CAPABILITY,
    CLOSE_VIEW_CAPABILITY,
    PANE_PREVIEW_CAPABILITY,
    TERMINAL_HISTORY_CAPABILITY,
    MOVE_TAB_CAPABILITY,
    #[cfg(target_os = "linux")]
    PAIR_CAPABILITY,
    STATUS_BAR_CAPABILITY,
    ADAPTIVE_VIEWPORT_CAPABILITY,
    #[cfg(target_os = "linux")]
    RESPONSE_NOTIFICATION_CAPABILITY,
    DEFAULT_SHELL_CAPABILITY,
    LEGACY_DEFAULT_LAUNCH_CAPABILITY,
    #[cfg(target_os = "linux")]
    "agent_launch.v1",
    #[cfg(windows)]
    corvid_protocol::PROCESS_AGENT_LAUNCH_CAPABILITY,
    #[cfg(target_os = "linux")]
    corvid_protocol::collector::CODEX_LAUNCH_CAPABILITY,
];
const MAX_INPUT_CHUNK: usize = 8192;

pub(crate) async fn serve(
    stream: Stream,
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
    let request = loop {
        let Some(request) = timeout(COMMAND_TIMEOUT, reader.next_envelope()).await?? else {
            return Ok(());
        };
        if let Some(Body::RestoreWorkspace(_)) = &request.body {
            let result = session.validate_request(&request).and_then(|body| {
                let Body::RestoreWorkspace(restore) = body else {
                    unreachable!()
                };
                workspaces.restore_workspace(
                    restore,
                    request.request_id.as_deref().expect("validated request ID"),
                )
            });
            let body = match result {
                Ok(result) => Body::WorkspaceRestored(result),
                Err(error) => Body::Error(error.into_wire()),
            };
            send(&session, &mut writer, request.request_id, body).await?;
            continue;
        }
        break request;
    };
    let client = Uuid::new_v4();
    let (focus_tx, focuses) = mpsc::channel(1);
    let (disconnect, disconnected) = oneshot::channel();
    let (notice_tx, notices) = mpsc::channel(8);
    let target = match session.validate_request(&request) {
        Ok(Body::Attach(attach)) => attach_target(attach).map(|target| {
            let enabled = attach
                .terminal_capabilities
                .iter()
                .any(|capability| capability == RESPONSE_NOTIFICATION_CAPABILITY);
            (target, enabled)
        }),
        Ok(_) => Err(invalid("expected Attach request")),
        Err(error) => Err(error),
    };
    let active = match target {
        Ok((target, enabled)) => {
            #[cfg(windows)]
            let _ = (enabled, notice_tx);
            workspaces
                .attach(
                    client,
                    target,
                    focus_tx,
                    Some(disconnect),
                    #[cfg(target_os = "linux")]
                    enabled.then_some(notice_tx),
                )
                .await
        }
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
        ClientEvents {
            focuses,
            notices,
            disconnected,
        },
    )
    .await;
    workspaces.release(client);
    result
}

async fn serve_active(
    session: Session,
    reader: FrameReader<ReadHalf>,
    mut writer: FrameWriter<WriteHalf>,
    request_id: Option<String>,
    active: Active,
    workspaces: &Workspaces,
    client_events: ClientEvents,
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
    serve_writer(session, reader, writer, active, workspaces, client_events).await
}

async fn serve_writer(
    session: Session,
    reader: FrameReader<ReadHalf>,
    writer: FrameWriter<WriteHalf>,
    active: Active,
    workspaces: &Workspaces,
    client_events: ClientEvents,
) -> Result<()> {
    TuiConnection {
        session,
        writer,
        active,
        workspaces,
        client_events,
        last_preview: None,
        last_history: None,
    }
    .run(reader)
    .await
}

struct TuiConnection<'a> {
    session: Session,
    writer: FrameWriter<WriteHalf>,
    active: Active,
    workspaces: &'a Workspaces,
    client_events: ClientEvents,
    last_preview: Option<Instant>,
    last_history: Option<Instant>,
}

struct ClientEvents {
    focuses: mpsc::Receiver<FocusDispatch>,
    notices: mpsc::Receiver<Provider>,
    disconnected: oneshot::Receiver<()>,
}

impl TuiConnection<'_> {
    async fn run(mut self, reader: FrameReader<ReadHalf>) -> Result<()> {
        let mut incoming = Incoming::start(reader);
        loop {
            tokio::select! {
                biased;
                _ = &mut self.client_events.disconnected => {
                    self.active.events.close();
                    self.send(None, Body::Detach(corvid_protocol::wire::Detach {})).await?;
                    return Ok(());
                }
                event = self.active.events.recv() => {
                    let Some(event) = event else { return Ok(()) };
                    if self.event(event).await? { return Ok(()) }
                }
                request = incoming.requests.recv() => {
                    let Some(request) = request else { return Ok(()) };
                    if self.request(request?).await? { return Ok(()) }
                }
                command = self.client_events.focuses.recv() => {
                    let Some(command) = command else { return Ok(()) };
                    if self.focus(command).await? { return Ok(()) }
                }
                Some(provider) = self.client_events.notices.recv() => {
                    self.send(None, Body::AgentResponded(AgentResponded {
                        provider: crate::projection::provider(provider) as i32,
                    })).await?;
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
            Body::CreateView(view) => {
                match create_destination(view, self.active.route.view_revision) {
                    Ok(destination) => self.switch(destination, request.request_id, false).await?,
                    Err(fault) => self.fault(request.request_id, fault).await?,
                }
            }
            Body::RenameView(view) => {
                let result =
                    require_view(view.expected_view_revision, self.active.route.view_revision)
                        .and_then(|_| {
                            if view.pane {
                                return Err(invalid("pane titles follow the terminal"));
                            }
                            self.workspaces.rename_tab(&self.active, view.name.clone())
                        });
                match result {
                    Ok(()) => {
                        self.send(
                            request.request_id,
                            Body::ViewRenamed(corvid_protocol::wire::ViewRenamed {}),
                        )
                        .await?
                    }
                    Err(fault) => self.fault(request.request_id, fault).await?,
                }
            }
            Body::CloseView(view) => match close_target(view, self.active.route.view_revision) {
                Ok(target) => return self.close(target, request.request_id, false).await,
                Err(fault) => self.fault(request.request_id, fault).await?,
            },
            Body::FocusViewAgent(agent) => {
                return self
                    .focus_own_agent(agent, request.request_id.clone())
                    .await;
            }
            Body::ReadPanePreview(preview) => {
                self.preview(preview, request.request_id.clone()).await?
            }
            Body::ReadTerminalHistory(history) => {
                let result = self.history(history.expected_view_revision).await;
                match result {
                    Ok(history) => {
                        self.send(request.request_id, Body::TerminalHistory(history))
                            .await?
                    }
                    Err(fault) => self.fault(request.request_id, fault).await?,
                }
            }
            Body::DisconnectWorkspaceClients(disconnect) => {
                match require_view(
                    disconnect.expected_view_revision,
                    self.active.route.view_revision,
                ) {
                    Ok(()) => self.disconnect_others(request.request_id).await?,
                    Err(fault) => self.fault(request.request_id, fault).await?,
                }
            }
            Body::ClaimWorkspaceWriter(claim) => {
                let workspace = parse_id(&claim.workspace_id);
                let result = match workspace {
                    Ok(workspace) => {
                        self.workspaces
                            .claim(client, workspace, self.active.viewport.content())
                    }
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
            Event::RenameTab(route, name) if route == self.active.route => {
                if let Err(fault) = self.workspaces.rename_tab(&self.active, name) {
                    self.fault(None, fault).await?;
                }
            }
            Event::RenameTab(_, _) => {}
            Event::Render(render) if render.route == self.active.route => {
                self.render(render).await?
            }
            Event::Render(_) => {}
            Event::ViewChanged { route, viewport } => {
                self.active.route = route;
                self.active.viewport.outer = viewport;
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
            Event::Close(target) => return self.close(target, None, true).await,
            Event::PairController => self.pair(None).await?,
            Event::DisconnectOthers => self.disconnect_others(None).await?,
        }
        Ok(false)
    }

    async fn disconnect_others(&mut self, id: Option<String>) -> Result<()> {
        match self.workspaces.disconnect_others(&self.active) {
            Ok(client_count) => {
                self.send(
                    id,
                    Body::WorkspaceClientsDisconnected(WorkspaceClientsDisconnected {
                        client_count,
                    }),
                )
                .await
            }
            Err(fault) => self.fault(id, fault).await,
        }
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

    async fn focus_own_agent(
        &mut self,
        agent: &FocusViewAgent,
        request_id: Option<String>,
    ) -> Result<bool> {
        let target = (|| {
            require_view(
                agent.expected_view_revision,
                self.active.route.view_revision,
            )?;
            let run = parse_id(&agent.agent_run_id)?;
            let view = agent
                .expected_location
                .as_ref()
                .ok_or_else(|| invalid("missing agent location"))?;
            let location = parse_selection(view)?;
            Ok((run, location))
        })();
        let (run, location) = match target {
            Ok(target) => target,
            Err(fault) => {
                self.fault(request_id, fault).await?;
                return Ok(false);
            }
        };
        match self
            .workspaces
            .focus_own_agent(&self.active, run, location)
            .await
        {
            Ok(focused) => {
                if let Some(active) = focused.active {
                    self.active = active;
                }
                self.view_changed(request_id, false).await?;
            }
            Err(rejected) => {
                if let FocusRecovery::Restored(route) = rejected.recovery {
                    self.active.route = route;
                    self.view_changed(None, false).await?;
                }
                self.fault(request_id, rejected.fault).await?;
                return Ok(matches!(rejected.recovery, FocusRecovery::Lost));
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

    async fn close(
        &mut self,
        target: CloseTarget,
        request_id: Option<String>,
        modal: bool,
    ) -> Result<bool> {
        match self.workspaces.close(&self.active, target).await {
            Ok(Closed::Switched(switched)) => {
                self.active = switched.active;
                self.view_changed(request_id, switched.paste_discarded)
                    .await?;
            }
            Ok(Closed::Unchanged) => {
                if modal {
                    self.active.terminal.resume(self.active.route).await?;
                }
                self.view_changed(request_id, false).await?;
            }
            Ok(Closed::Detached) => {
                self.send(request_id, Body::Detach(corvid_protocol::wire::Detach {}))
                    .await?;
                return Ok(true);
            }
            Err(fault) => {
                if modal {
                    self.active.terminal.resume(self.active.route).await?;
                }
                self.fault(request_id, fault).await?;
            }
        }
        Ok(false)
    }

    async fn view_changed(
        &mut self,
        request_id: Option<String>,
        paste_discarded: bool,
    ) -> Result<()> {
        let viewport = self.active.viewport.outer;
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
            preview: None,
        };
        let frame = RenderFrame {
            header,
            ansi: render.ansi,
        };
        timeout(COMMAND_TIMEOUT, self.writer.render(&frame)).await??;
        Ok(())
    }

    async fn preview(&mut self, preview: &ReadPanePreview, id: Option<String>) -> Result<()> {
        let outcome = async {
            require_view(
                preview.expected_view_revision,
                self.active.route.view_revision,
            )?;
            let pane = parse_id(&preview.pane_id)?;
            let now = Instant::now();
            if self
                .last_preview
                .is_some_and(|last| now.duration_since(last) < Duration::from_millis(180))
            {
                return Err(ProtocolFault::new(
                    "LimitExceeded",
                    "pane previews are rate limited",
                ));
            }
            self.last_preview = Some(now);
            timeout(
                Duration::from_millis(150),
                self.workspaces.preview(&self.active, pane),
            )
            .await
            .map_err(|_| ProtocolFault::new("Timeout", "pane preview timed out"))?
        }
        .await;
        let (viewport, ansi) = match outcome {
            Ok(preview) => preview,
            Err(fault) => return self.fault(id, fault).await,
        };
        let header = RenderHeader {
            host_id: self.session.host_id.clone(),
            server_epoch: self.session.server_epoch.clone(),
            client_id: self.active.route.client_id.to_string(),
            view_revision: self.active.route.view_revision,
            render_generation: 1,
            frame_id: 1,
            base_frame_id: None,
            ansi_bytes: ansi.len() as u32,
            preview: Some(PanePreviewFrame {
                pane_id: preview.pane_id.clone(),
                viewport: Some(WireViewport {
                    rows: viewport.rows.get().into(),
                    cols: viewport.cols.get().into(),
                }),
            }),
        };
        timeout(
            COMMAND_TIMEOUT,
            self.writer.render(&RenderFrame { header, ansi }),
        )
        .await??;
        Ok(())
    }

    async fn history(
        &mut self,
        revision: Option<u64>,
    ) -> Result<corvid_protocol::wire::TerminalHistory, ProtocolFault> {
        let route = self.active.route;
        require_view(revision, route.view_revision)?;
        let now = Instant::now();
        if self
            .last_history
            .is_some_and(|last| now.duration_since(last) < Duration::from_millis(180))
        {
            return Err(ProtocolFault::new(
                "LimitExceeded",
                "terminal history reads are rate limited",
            ));
        }
        self.last_history = Some(now);
        self.active.terminal.history(route).await
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

fn create_destination(view: &CreateView, revision: u64) -> Result<Destination, ProtocolFault> {
    use corvid_protocol::wire::create_view::Target;

    require_view(view.expected_view_revision, revision)?;
    match view
        .target
        .as_ref()
        .ok_or_else(|| invalid("missing view target"))?
    {
        Target::WorkspaceLabel(label) => Ok(Destination::CreateWorkspace(label.clone())),
        Target::TabLabel(label) if label.is_empty() => {
            Ok(Destination::Tab(crate::terminal::TabCommand::Create))
        }
        Target::TabLabel(label) => Ok(Destination::CreateTab(label.clone())),
        Target::SplitAxis(axis) => match SplitAxis::try_from(*axis) {
            Ok(SplitAxis::Horizontal) => Ok(Destination::Split(Axis::Horizontal)),
            Ok(SplitAxis::Vertical) => Ok(Destination::Split(Axis::Vertical)),
            _ => Err(invalid("invalid split axis")),
        },
        Target::OpenPane(view) => Ok(Destination::Open(parse_selection(view)?)),
    }
}

fn parse_selection(view: &corvid_protocol::wire::ViewSelection) -> Result<Location, ProtocolFault> {
    Ok(Location {
        workspace: parse_id(&view.workspace_id)?,
        tab: parse_id(&view.tab_id)?,
        pane: parse_id(&view.pane_id)?,
    })
}

fn close_target(view: &CloseView, revision: u64) -> Result<CloseTarget, ProtocolFault> {
    require_view(view.expected_view_revision, revision)?;
    let workspace = parse_id(&view.workspace_id)?;
    match (&view.tab_id, &view.pane_id) {
        (None, None) => Ok(CloseTarget::Workspace(workspace)),
        (Some(tab), None) => Ok(CloseTarget::Tab {
            workspace,
            tab: parse_id(tab)?,
        }),
        (Some(tab), Some(pane)) => Ok(CloseTarget::Pane(Location {
            workspace,
            tab: parse_id(tab)?,
            pane: parse_id(pane)?,
        })),
        (None, Some(_)) => Err(invalid("closing a pane requires its tab ID")),
    }
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
    writer: &mut FrameWriter<WriteHalf>,
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

    // Older clients can request the same conditional shell with the old flag.
    let launch_if_empty = attach.terminal_capabilities.iter().any(|capability| {
        matches!(
            capability.as_str(),
            DEFAULT_SHELL_CAPABILITY | LEGACY_DEFAULT_LAUNCH_CAPABILITY
        )
    });
    if launch_if_empty
        && (attach.workspace_id.is_some()
            || attach.tab_id.is_some()
            || attach.pane_id.is_some()
            || attach.create_workspace.is_some()
            || attach.create_tab.is_some()
            || attach
                .launch
                .as_ref()
                .is_none_or(|launch| !launch.argv.is_empty() || launch.agent_provider.is_some()))
    {
        return Err(invalid(
            "default shell requires the default workspace and no program",
        ));
    }
    if attach.take_over
        && (attach.requested_role != AttachRole::Writer as i32
            || (attach.launch.is_some() && !launch_if_empty)
            || attach.create_workspace.is_some()
            || attach.create_tab.is_some()
            || !attach
                .terminal_capabilities
                .iter()
                .any(|cap| cap == corvid_protocol::PANE_WRITERS_CAPABILITY))
    {
        return Err(invalid(
            "pane takeover requires a writer attach to an existing pane",
        ));
    }
    if attach.expected_terminal_id.is_some() && (!attach.take_over || attach.pane_id.is_none()) {
        return Err(invalid("terminal identity requires an exact pane takeover"));
    }
    Ok(AttachTarget {
        workspace: parse_optional_id(attach.workspace_id.as_deref())?,
        tab: parse_optional_id(attach.tab_id.as_deref())?,
        pane: parse_optional_id(attach.pane_id.as_deref())?,
        new_workspace: attach.create_workspace.clone(),
        new_tab: attach.create_tab.clone(),
        take_over: attach.take_over,
        expected_terminal: parse_optional_id(attach.expected_terminal_id.as_deref())?,
        keymap: Keymap {
            naming: attach
                .terminal_capabilities
                .iter()
                .any(|name| name == corvid_protocol::VIEW_TITLES_CAPABILITY),
            disconnect: attach
                .terminal_capabilities
                .iter()
                .any(|cap| cap == corvid_protocol::SESSION_CLIENTS_CAPABILITY),
            pane_writer: attach
                .terminal_capabilities
                .iter()
                .any(|name| name == corvid_protocol::PANE_WRITERS_CAPABILITY),
            tabs: attach
                .terminal_capabilities
                .iter()
                .any(|name| name == "tabs.v1"),
            pairing: attach
                .terminal_capabilities
                .iter()
                .any(|name| name == PAIR_CAPABILITY),
            panes: attach
                .terminal_capabilities
                .iter()
                .any(|name| name == CREATE_VIEW_CAPABILITY),
            close: attach
                .terminal_capabilities
                .iter()
                .any(|name| name == CLOSE_VIEW_CAPABILITY),
        },
        viewport: TuiViewport {
            outer: viewport(attach.viewport.as_ref())?,
            status_bar: attach
                .terminal_capabilities
                .iter()
                .any(|name| name == STATUS_BAR_CAPABILITY),
        },
        launch: attach
            .launch
            .as_ref()
            .map(|command| launch(command, launch_if_empty))
            .transpose()?,
    })
}

fn parse_optional_id(id: Option<&str>) -> Result<Option<Uuid>, ProtocolFault> {
    id.map(Uuid::parse_str)
        .transpose()
        .map_err(|_| invalid("invalid layout ID"))
}

fn viewport(value: Option<&WireViewport>) -> Result<Viewport, ProtocolFault> {
    let value = value.ok_or_else(|| invalid("missing viewport"))?;
    let error = || {
        invalid(&format!(
            "viewport {}×{} requires 2–65535 columns and rows",
            value.cols, value.rows
        ))
    };
    let cols = u16::try_from(value.cols).map_err(|_| error())?;
    let rows = u16::try_from(value.rows).map_err(|_| error())?;
    Viewport::new(cols, rows).map_err(|_| error())
}

fn launch(command: &LaunchCommand, if_empty: bool) -> Result<Launch, ProtocolFault> {
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
        .map(|bytes| {
            decode_os_string(bytes)
                .map(PathBuf::from)
                .ok_or_else(|| invalid("invalid launch working directory encoding"))
        })
        .transpose()?;
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
            .map(|bytes| {
                decode_os_string(bytes).ok_or_else(|| invalid("invalid launch argument encoding"))
            })
            .collect::<Result<_, _>>()?,
        cwd,
        provider,
        if_empty,
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
