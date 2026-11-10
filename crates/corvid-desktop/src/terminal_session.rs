use std::collections::HashMap;
use std::num::NonZeroU16;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use corvid_client::{ControlEndpoint, InteractiveConnection, SshAlias};
use corvid_protocol::wire::create_view::Target;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::layout_node_state::Body as Node;
use corvid_protocol::wire::snapshot_entity::Body as EntityBody;
use corvid_protocol::wire::{
    AgentInfo, Attach, AttachRole, ClaimWorkspaceWriter, CreateView, FocusViewAgent, FrameApplied,
    InputChunk, LaunchCommand, PasteBegin, PasteChunk, PasteEnd, ReadPanePreview, ResizeViewport,
    RouteToken, SelectView, TerminalState, ViewSelection, Viewport, WorkspaceState,
};
use corvid_protocol::{
    CLAIM_WORKSPACE_CAPABILITY, COMMAND_TIMEOUT, CREATE_VIEW_CAPABILITY, DEFAULT_SHELL_CAPABILITY,
    Frame, PANE_PREVIEW_CAPABILITY, ProtocolFault, RenderFrame, decode_envelope,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, timeout};
use uuid::Uuid;
use vt100::{Parser, Screen};

use crate::{slot_board, ssh_bootstrap};

pub const MAX_INPUT: usize = 64 * 1024;
const INPUT_CHUNK: usize = 8192;
const PREVIEW_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    pub rows: NonZeroU16,
    pub cols: NonZeroU16,
}

impl Grid {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows: NonZeroU16::new(rows.clamp(2, 100)).unwrap(),
            cols: NonZeroU16::new(cols.clamp(2, 300)).unwrap(),
        }
    }

    fn wire(self) -> Viewport {
        Viewport {
            cols: self.cols.get().into(),
            rows: self.rows.get().into(),
        }
    }

    fn read(viewport: Viewport) -> Result<Self> {
        let grid = Self::new(viewport.rows.try_into()?, viewport.cols.try_into()?);
        ensure!(
            viewport == grid.wire(),
            "daemon returned an unsupported viewport"
        );
        Ok(grid)
    }
}

#[derive(Clone, Default)]
pub struct Inventory {
    pub workspaces: Vec<WorkspaceState>,
    pub terminals: Vec<TerminalState>,
    pub agents: Vec<AgentInfo>,
}

impl Inventory {
    pub fn live(&self, selection: &ViewSelection) -> bool {
        self.terminals
            .iter()
            .any(|terminal| terminal.location.as_ref() == Some(selection))
    }

    fn live_siblings(&self, selected: &ViewSelection) -> Vec<String> {
        let Some(tab) = self
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == selected.workspace_id)
            .and_then(|workspace| {
                workspace
                    .tabs
                    .iter()
                    .find(|tab| tab.tab_id == selected.tab_id)
            })
        else {
            return Vec::new();
        };
        tab.nodes
            .iter()
            .filter_map(|node| match node.body.as_ref() {
                Some(Node::PaneId(pane))
                    if *pane != selected.pane_id
                        && self.live(&ViewSelection {
                            pane_id: pane.clone(),
                            ..selected.clone()
                        }) =>
                {
                    Some(pane.clone())
                }
                _ => None,
            })
            .collect()
    }
}

#[derive(Clone)]
pub struct Ui {
    pub selected: Option<ViewSelection>,
    pub inventory: Option<Arc<Inventory>>,
    pub inventory_ready: bool,
    pub message: String,
    pub connected: bool,
    pub changing: bool,
}

struct Buffer {
    ui: Ui,
    parser: Parser,
    revision: u64,
    displayed: bool,
    previews: HashMap<String, Screen>,
}

struct Shared {
    buffer: Mutex<Buffer>,
    screen_changed: watch::Sender<()>,
    ui_changed: watch::Sender<()>,
}

impl Shared {
    fn buffer(&self) -> MutexGuard<'_, Buffer> {
        self.buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn update_ui(&self, edit: impl FnOnce(&mut Ui)) {
        edit(&mut self.buffer().ui);
        self.ui_changed.send_replace(());
    }
}

#[derive(Clone)]
pub struct Session {
    shared: Arc<Shared>,
    actions: mpsc::Sender<Action>,
    size: watch::Sender<(u64, Grid)>,
}

pub struct Job {
    stop: Option<oneshot::Sender<()>>,
    thread: thread::JoinHandle<()>,
}

impl Job {
    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }

    pub fn finished(&self) -> bool {
        self.thread.is_finished()
    }

    pub fn join(self) {
        let _ = self.thread.join();
    }
}

enum Action {
    Input {
        revision: u64,
        bytes: Vec<u8>,
        paste: bool,
    },
    Select(ViewSelection, bool, Option<String>),
    Create(Target),
    OpenSlotBoard(slot_board::Settings),
}

struct Requests {
    actions: mpsc::Receiver<Action>,
    sizes: watch::Receiver<(u64, Grid)>,
}

impl Session {
    pub fn start(
        alias: SshAlias,
        expected_host: Option<String>,
        selection: Option<(ViewSelection, bool)>,
        explicit: bool,
    ) -> Result<(Self, Job)> {
        let grid = Grid::new(26, 112);
        let (screen_changed, _) = watch::channel(());
        let (ui_changed, _) = watch::channel(());
        let shared = Arc::new(Shared {
            buffer: Mutex::new(Buffer {
                ui: Ui {
                    selected: None,
                    inventory: None,
                    inventory_ready: false,
                    message: "Connecting to Corvid…".into(),
                    connected: false,
                    changing: true,
                },
                parser: Parser::new(grid.rows, grid.cols, 0),
                revision: 0,
                displayed: false,
                previews: HashMap::new(),
            }),
            screen_changed,
            ui_changed,
        });
        let (actions, incoming) = mpsc::channel(32);
        let (size, sizes) = watch::channel((0, grid));
        let requests = Requests {
            actions: incoming,
            sizes,
        };
        let (stop, mut stopped) = oneshot::channel();
        let state = Arc::clone(&shared);
        let thread = thread::Builder::new().name("corvid-desktop-client".into()).spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread().enable_all().build()
                .map_err(anyhow::Error::from)
                .and_then(|runtime| runtime.block_on(async {
                    let mut slot_board = slot_board::Job::default();
                    let result = tokio::select! {
                        result = run(alias, expected_host, selection, explicit, requests, &state, &mut slot_board) => result,
                        _ = &mut stopped => Ok("Detached from Corvid.".into()),
                    };
                    slot_board.close().await;
                    result
                }));
            state.update_ui(|ui| {
                ui.connected = false;
                ui.inventory_ready = false;
                ui.changing = false;
                ui.message = match result {
                    Ok(message) => message,
                    Err(error) => format!("{error:#}"),
                };
            });
        })?;
        Ok((
            Self {
                shared,
                actions,
                size,
            },
            Job {
                stop: Some(stop),
                thread,
            },
        ))
    }

    pub fn ui(&self) -> Ui {
        self.shared.buffer().ui.clone()
    }

    pub fn changes(&self) -> watch::Receiver<()> {
        self.shared.ui_changed.subscribe()
    }

    pub fn screen_changes(&self) -> watch::Receiver<()> {
        self.shared.screen_changed.subscribe()
    }

    pub fn screen<R>(&self, pane: &str, read: impl FnOnce(&Screen) -> R) -> Option<R> {
        let buffer = self.shared.buffer();
        (buffer.displayed && in_pane(&buffer.ui, pane)).then(|| read(buffer.parser.screen()))
    }

    pub fn preview<R>(&self, pane: &str, read: impl FnOnce(&Screen) -> R) -> Option<R> {
        self.shared.buffer().previews.get(pane).map(read)
    }

    pub fn resize(&self, pane: &str, grid: Grid) {
        let buffer = self.shared.buffer();
        if !in_pane(&buffer.ui, pane) {
            return;
        }
        let revision = buffer.revision;
        self.size.send_if_modified(|last| {
            let desired = (revision, grid);
            let changed = *last != desired;
            *last = desired;
            changed
        });
    }

    pub fn send(&self, pane: &str, bytes: Vec<u8>, paste: bool) -> bool {
        let buffer = self.shared.buffer();
        if !buffer.ui.connected
            || buffer.ui.changing
            || !buffer.displayed
            || !in_pane(&buffer.ui, pane)
            || bytes.is_empty()
            || bytes.len() > MAX_INPUT
        {
            return false;
        }
        self.actions
            .try_send(Action::Input {
                revision: buffer.revision,
                bytes,
                paste,
            })
            .is_ok()
    }

    pub fn select(&self, selection: ViewSelection, open: bool, run: Option<String>) {
        self.change(Action::Select(selection, open, run));
    }

    pub fn create(&self, target: Target) {
        self.change(Action::Create(target));
    }

    pub fn open_slot_board(&self, settings: slot_board::Settings) {
        if self.shared.buffer().ui.connected {
            let _ = self.actions.try_send(Action::OpenSlotBoard(settings));
        }
    }

    fn change(&self, action: Action) {
        let mut buffer = self.shared.buffer();
        if !buffer.ui.connected || buffer.ui.changing {
            return;
        }
        if self.actions.try_send(action).is_ok() {
            buffer.ui.changing = true;
            self.shared.ui_changed.send_replace(());
        }
    }
}

fn in_pane(ui: &Ui, pane: &str) -> bool {
    ui.selected
        .as_ref()
        .is_some_and(|selection| selection.pane_id == pane)
}

struct Client<'a> {
    link: InteractiveConnection,
    alias: SshAlias,
    slot_board: &'a mut slot_board::Job,
    shared: &'a Shared,
    selected: ViewSelection,
    route: RouteToken,
    grid: Grid,
    applied: Option<(u64, u64)>,
    sequence: u64,
    pending: Option<Pending>,
    preview: Option<(String, Pending)>,
    preview_index: usize,
    next_preview: Instant,
}

struct Pending {
    id: String,
    deadline: Instant,
    kind: Operation,
}

enum Operation {
    Claim(ViewSelection, bool, Option<String>),
    View,
    Resize,
    Preview,
}

async fn run(
    alias: SshAlias,
    expected_host: Option<String>,
    selection: Option<(ViewSelection, bool)>,
    explicit: bool,
    Requests { mut actions, sizes }: Requests,
    shared: &Shared,
    slot_board: &mut slot_board::Job,
) -> Result<String> {
    let capabilities = [
        "attach.v1",
        "render.v1",
        "paste.v1",
        "select_view.v1",
        CREATE_VIEW_CAPABILITY,
        CLAIM_WORKSPACE_CAPABILITY,
        PANE_PREVIEW_CAPABILITY,
        DEFAULT_SHELL_CAPABILITY,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let (alias, mut link) = ssh_bootstrap::connect(
        alias,
        capabilities,
        explicit,
        expected_host.as_deref(),
        |message| shared.update_ui(|ui| ui.message = message.into()),
    )
    .await?;
    ensure!(
        expected_host
            .as_ref()
            .is_none_or(|host| *host == link.session().host_id),
        "SSH alias points to a different Corvid host"
    );
    let grid = sizes.borrow().1;
    let target = selection.as_ref().map(|(selection, _)| selection);
    let mut terminal_capabilities = vec!["legacy_vt.v1".into()];
    if target.is_none() {
        terminal_capabilities.push(DEFAULT_SHELL_CAPABILITY.into());
    }
    let open_shell = selection.as_ref().is_none_or(|(_, open)| *open);
    let id = link
        .send(Body::Attach(Attach {
            workspace_id: target.map(|view| view.workspace_id.clone()),
            tab_id: target.map(|view| view.tab_id.clone()),
            pane_id: target.map(|view| view.pane_id.clone()),
            viewport: Some(grid.wire()),
            terminal_capabilities,
            requested_role: AttachRole::Writer as i32,
            launch: open_shell.then(LaunchCommand::default),
            ..Default::default()
        }))
        .await?;
    let reply = timeout(COMMAND_TIMEOUT, link.receive()).await??;
    let (reply_id, reply) = envelope(&link, reply)?;
    ensure!(
        reply_id.as_deref() == Some(&id),
        "unexpected attach response ID"
    );
    let attached = match reply {
        Body::Attached(attached) => attached,
        Body::Error(error) => return Err(ProtocolFault::from_wire(&error).into()),
        _ => bail!("unexpected attach response"),
    };
    let route = attached.route_token.context("missing input route")?;
    let selected = attached.selection.context("missing attached view")?;
    validate_selection(&selected)?;
    ensure!(
        route.view_revision == 1
            && route.connection_generation > 0
            && route.connection_generation == attached.connection_generation
            && route.client_id == attached.client_id
            && valid_route(&route)
            && target.is_none_or(|target| target == &selected),
        "invalid initial daemon route"
    );

    let host = link.session().host_id.clone();
    let epoch = link.session().server_epoch.clone();
    let mut client = Client {
        link,
        alias: alias.clone(),
        slot_board,
        shared,
        selected,
        route,
        grid,
        applied: None,
        sequence: 0,
        pending: None,
        preview: None,
        preview_index: 0,
        next_preview: Instant::now(),
    };
    client.publish_view();
    let inventory = watch_inventory(alias, host, epoch, shared);
    tokio::pin!(inventory);
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            frame = client.link.receive() => {
                if let Some(message) = client.receive(frame?).await? {
                    return Ok(message);
                }
            }
            action = actions.recv() => match action {
                Some(action) => client.action(action).await?,
                None => return Ok("Detached from Corvid.".into()),
            },
            result = client.slot_board.tasks.join_next(), if !client.slot_board.tasks.is_empty() => {
                match result {
                    Some(Ok(Err(error))) => shared.update_ui(|ui| ui.message = format!("{error:#}")),
                    Some(Err(_)) => shared.update_ui(|ui| ui.message = "Slot Board stopped unexpectedly.".into()),
                    _ => {}
                }
            }
            _ = tick.tick() => {
                let size = *sizes.borrow();
                client.tick(size).await?;
            }
            _ = &mut inventory => unreachable!("inventory watcher retries until detached"),
        }
    }
}

async fn watch_inventory(alias: SshAlias, host: String, epoch: String, shared: &Shared) {
    let endpoint = ControlEndpoint::Ssh(alias);
    let controller = Uuid::new_v4().to_string();
    loop {
        let _ = watch_inventory_once(&endpoint, &controller, &host, &epoch, shared).await;
        shared.update_ui(|ui| ui.inventory_ready = false);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn watch_inventory_once(
    endpoint: &ControlEndpoint,
    controller: &str,
    host: &str,
    epoch: &str,
    shared: &Shared,
) -> Result<()> {
    let mut watch = endpoint.watch(controller, host, || {}).await?;
    ensure!(
        watch.state().session().server_epoch == epoch,
        "daemon restarted during attach"
    );
    loop {
        let mut inventory = Inventory::default();
        for entity in watch.state().snapshot().entities() {
            match &entity.body {
                Some(EntityBody::Workspace(workspace)) => {
                    inventory.workspaces.push(workspace.clone())
                }
                Some(EntityBody::Terminal(terminal)) => inventory.terminals.push(terminal.clone()),
                Some(EntityBody::Agent(agent)) if agent.parent_run_id.is_none() => {
                    inventory.agents.extend(agent.summary.clone());
                }
                _ => {}
            }
        }
        {
            let mut buffer = shared.buffer();
            buffer.previews.retain(|pane, _| {
                inventory.terminals.iter().any(|terminal| {
                    terminal
                        .location
                        .as_ref()
                        .is_some_and(|location| &location.pane_id == pane)
                })
            });
            buffer.ui.inventory = Some(Arc::new(inventory));
            buffer.ui.inventory_ready = true;
        }
        shared.ui_changed.send_replace(());
        watch.next_change().await?;
    }
}

fn envelope(link: &InteractiveConnection, frame: Frame) -> Result<(Option<String>, Body)> {
    let Frame::Structured(payload) = frame else {
        bail!("expected structured daemon reply")
    };
    let message = decode_envelope(&payload)?;
    link.session().validate_scope(&message)?;
    Ok((
        message.request_id,
        message.body.context("missing daemon message")?,
    ))
}

fn valid_route(route: &RouteToken) -> bool {
    route.writer_lease_generation > 0
        && Uuid::parse_str(&route.client_id).is_ok()
        && Uuid::parse_str(&route.terminal_id).is_ok()
}

fn validate_selection(view: &ViewSelection) -> Result<()> {
    Uuid::parse_str(&view.workspace_id)?;
    Uuid::parse_str(&view.tab_id)?;
    Uuid::parse_str(&view.pane_id)?;
    Ok(())
}

impl Client<'_> {
    fn publish_view(&self) {
        {
            let mut buffer = self.shared.buffer();
            if buffer
                .ui
                .selected
                .as_ref()
                .is_some_and(|previous| previous.tab_id != self.selected.tab_id)
            {
                buffer.previews.clear();
            }
            buffer.ui.selected = Some(self.selected.clone());
            buffer.ui.connected = true;
            buffer.ui.changing = self.pending.is_some();
            buffer.ui.message = "Attached to Corvid daemon".into();
            buffer.revision = self.route.view_revision;
            buffer.displayed = false;
            buffer.parser = Parser::new(self.grid.rows, self.grid.cols, 0);
        }
        self.shared.ui_changed.send_replace(());
        self.shared.screen_changed.send_replace(());
    }

    async fn tick(&mut self, (revision, grid): (u64, Grid)) -> Result<()> {
        let now = Instant::now();
        if let Some((_, preview)) = &self.preview {
            ensure!(now < preview.deadline, "daemon pane preview timed out");
        }
        if let Some(pending) = &self.pending {
            ensure!(now < pending.deadline, "daemon view change timed out");
        } else if revision == self.route.view_revision && self.grid != grid {
            self.resize(grid).await?;
        } else if self.preview.is_none() && now >= self.next_preview && self.applied.is_some() {
            self.next_preview = now + PREVIEW_INTERVAL;
            self.read_preview().await?;
        }
        Ok(())
    }

    async fn read_preview(&mut self) -> Result<()> {
        let panes = self
            .shared
            .buffer()
            .ui
            .inventory
            .as_ref()
            .map_or_else(Vec::new, |inventory| {
                inventory.live_siblings(&self.selected)
            });
        if panes.is_empty() {
            return Ok(());
        }
        let pane = panes[self.preview_index % panes.len()].clone();
        self.preview_index = self.preview_index.wrapping_add(1);
        let id = self
            .link
            .send(Body::ReadPanePreview(ReadPanePreview {
                expected_view_revision: Some(self.route.view_revision),
                pane_id: pane.clone(),
            }))
            .await?;
        self.preview = Some((
            pane,
            Pending {
                id,
                deadline: Instant::now() + COMMAND_TIMEOUT,
                kind: Operation::Preview,
            },
        ));
        Ok(())
    }

    async fn action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::OpenSlotBoard(settings) if self.slot_board.tasks.is_empty() => {
                let alias = self.alias.clone();
                let host = self.link.session().host_id.clone();
                self.slot_board.spawn(settings, alias, host);
            }
            Action::OpenSlotBoard(_) => self.shared.update_ui(|ui| {
                ui.message = "Slot Board is already open or starting.".into();
            }),
            Action::Input {
                revision,
                bytes,
                paste,
            } if self.pending.is_none()
                && self.applied.is_some()
                && revision == self.route.view_revision =>
            {
                self.input(bytes, paste).await?;
            }
            Action::Input { .. } => {}
            Action::Select(selection, open, run) => {
                if let Some(run) = &run {
                    let current = self.has_run(run, &selection);
                    if !current {
                        self.shared.update_ui(|ui| {
                            ui.changing = false;
                            ui.message = "The selected agent has moved or ended.".into();
                        });
                        return Ok(());
                    }
                }
                if selection.workspace_id != self.selected.workspace_id {
                    let body = Body::ClaimWorkspaceWriter(ClaimWorkspaceWriter {
                        workspace_id: selection.workspace_id.clone(),
                    });
                    self.request(body, Operation::Claim(selection, open, run))
                        .await?;
                } else {
                    self.select(selection, open, run).await?;
                }
            }
            Action::Create(target) => self.create(target).await?,
        }
        Ok(())
    }

    fn has_run(&self, run: &str, selection: &ViewSelection) -> bool {
        let buffer = self.shared.buffer();
        buffer.ui.inventory_ready
            && buffer.ui.inventory.as_ref().is_some_and(|inventory| {
                inventory.agents.iter().any(|agent| {
                    agent.agent_run_id == run
                        && agent.terminal_available
                        && agent.location.as_ref() == Some(selection)
                })
            })
    }

    async fn select(
        &mut self,
        selection: ViewSelection,
        open: bool,
        run: Option<String>,
    ) -> Result<()> {
        if let Some(run) = run {
            return self
                .request(
                    Body::FocusViewAgent(FocusViewAgent {
                        expected_view_revision: Some(self.route.view_revision),
                        agent_run_id: run,
                        expected_location: Some(selection),
                    }),
                    Operation::View,
                )
                .await;
        }
        if open {
            return self.create(Target::OpenPane(selection)).await;
        }
        self.request(
            Body::SelectView(SelectView {
                expected_view_revision: Some(self.route.view_revision),
                workspace_id: selection.workspace_id,
                tab_id: selection.tab_id,
                pane_id: selection.pane_id,
            }),
            Operation::View,
        )
        .await
    }

    async fn create(&mut self, target: Target) -> Result<()> {
        self.request(
            Body::CreateView(CreateView {
                expected_view_revision: Some(self.route.view_revision),
                target: Some(target),
            }),
            Operation::View,
        )
        .await
    }

    async fn resize(&mut self, grid: Grid) -> Result<()> {
        {
            let mut buffer = self.shared.buffer();
            if buffer.ui.changing {
                return Ok(());
            }
            buffer.ui.changing = true;
        }
        self.request(
            Body::ResizeViewport(ResizeViewport {
                client_id: self.route.client_id.clone(),
                connection_generation: self.route.connection_generation,
                viewport: Some(grid.wire()),
            }),
            Operation::Resize,
        )
        .await
    }

    async fn request(&mut self, body: Body, kind: Operation) -> Result<()> {
        ensure!(
            self.pending.is_none(),
            "another daemon view operation is pending"
        );
        let id = self.link.send(body).await?;
        self.pending = Some(Pending {
            id,
            deadline: Instant::now() + COMMAND_TIMEOUT,
            kind,
        });
        Ok(())
    }

    async fn input(&mut self, bytes: Vec<u8>, paste: bool) -> Result<()> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .context("input sequence exhausted")?;
        if paste {
            let id = Uuid::new_v4().to_string();
            self.link
                .send(Body::PasteBegin(PasteBegin {
                    route_token: Some(self.route.clone()),
                    paste_id: id.clone(),
                    input_sequence: self.sequence,
                }))
                .await?;
            for chunk in bytes.chunks(INPUT_CHUNK) {
                self.link
                    .send(Body::PasteChunk(PasteChunk {
                        paste_id: id.clone(),
                        data: chunk.to_vec(),
                    }))
                    .await?;
            }
            self.link
                .send(Body::PasteEnd(PasteEnd { paste_id: id }))
                .await?;
        } else {
            ensure!(bytes.len() <= INPUT_CHUNK, "terminal key input too large");
            self.link
                .send(Body::InputChunk(InputChunk {
                    route_token: Some(self.route.clone()),
                    input_sequence: self.sequence,
                    data: bytes,
                }))
                .await?;
        }
        Ok(())
    }

    async fn receive(&mut self, frame: Frame) -> Result<Option<String>> {
        let frame = match frame {
            Frame::Render(frame) => {
                self.render(frame).await?;
                return Ok(None);
            }
            frame => frame,
        };
        let (id, body) = envelope(&self.link, frame)?;
        match body {
            Body::ViewChanged(view) => {
                let route = view.route_token.context("view update has no route")?;
                let selection = view.selection.context("view update has no selection")?;
                validate_selection(&selection)?;
                ensure!(
                    view.view_revision == route.view_revision
                        && valid_route(&route)
                        && self.route.client_id == route.client_id
                        && self.route.connection_generation == route.connection_generation
                        && (route == self.route
                            || self.route.view_revision.checked_add(1)
                                == Some(route.view_revision)),
                    "invalid daemon view update"
                );
                let pending = self.pending.as_ref().is_some_and(|pending| {
                    id.as_deref() == Some(&pending.id)
                        || (id.is_none() && matches!(pending.kind, Operation::Resize))
                });
                if pending {
                    self.pending = None;
                }
                if route != self.route {
                    self.grid = Grid::read(view.viewport.context("view update has no viewport")?)?;
                    self.route = route;
                    self.selected = selection;
                    self.applied = None;
                    self.publish_view();
                } else if pending {
                    self.shared.update_ui(|ui| ui.changing = false);
                }
            }
            Body::WriterClaimed(claim) => {
                let pending = self.pending.take().context("unsolicited writer claim")?;
                let Operation::Claim(selection, open, run) = pending.kind else {
                    bail!("unexpected writer claim");
                };
                ensure!(
                    id.as_deref() == Some(&pending.id)
                        && claim.workspace_id == selection.workspace_id
                        && claim.generation > 0,
                    "invalid writer claim"
                );
                if run.as_ref().is_none_or(|run| self.has_run(run, &selection)) {
                    self.select(selection, open, run).await?;
                } else {
                    self.shared.update_ui(|ui| {
                        ui.changing = false;
                        ui.message = "The selected agent has moved or ended.".into();
                    });
                }
            }
            Body::Error(error) => {
                if self
                    .preview
                    .as_ref()
                    .is_some_and(|(_, preview)| id.as_deref() == Some(&preview.id))
                {
                    self.preview = None;
                    return Ok(None);
                }
                let matched = self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| id.as_deref() == Some(&pending.id));
                if matched {
                    self.pending = None;
                }
                self.shared.update_ui(|ui| {
                    if matched {
                        ui.changing = false;
                    }
                    ui.message = ProtocolFault::from_wire(&error).to_string();
                });
            }
            Body::TerminalExited(exit) if exit.terminal_id == self.route.terminal_id => {
                self.shared.update_ui(|ui| {
                    if let Some(inventory) = &mut ui.inventory {
                        Arc::make_mut(inventory)
                            .terminals
                            .retain(|terminal| terminal.terminal_id != exit.terminal_id);
                    }
                });
                return Ok(Some(format!("Shell exited ({}).", exit.exit_code)));
            }
            Body::Detach(_) => return Ok(Some("Detached from Corvid.".into())),
            _ => bail!("unexpected interactive daemon reply"),
        }
        Ok(None)
    }

    async fn render(&mut self, frame: RenderFrame) -> Result<()> {
        let header = frame.header;
        ensure!(
            header.host_id == self.link.session().host_id
                && header.server_epoch == self.link.session().server_epoch
                && header.client_id == self.route.client_id
                && header.view_revision == self.route.view_revision,
            "daemon render targets another view"
        );
        if let Some(preview) = header.preview {
            let (expected, _) = self.preview.take().context("unsolicited pane preview")?;
            ensure!(
                preview.pane_id == expected,
                "daemon returned a different pane preview"
            );
            if let Ok(grid) = Grid::read(preview.viewport.context("pane preview has no size")?) {
                let mut parser = Parser::new(grid.rows, grid.cols, 0);
                parser.process(&frame.ansi);
                self.shared
                    .buffer()
                    .previews
                    .insert(expected, parser.screen().clone());
                self.shared.ui_changed.send_replace(());
            }
            return Ok(());
        }
        match (self.applied, header.base_frame_id) {
            (None, None) => {}
            (Some((generation, previous)), Some(base)) => {
                ensure!(
                    generation == header.render_generation && previous == base,
                    "daemon render base mismatch"
                );
            }
            (Some((generation, previous)), None) => {
                ensure!(
                    header.render_generation > generation && header.frame_id > previous,
                    "stale daemon redraw"
                );
            }
            _ => bail!("daemon must start with a full render"),
        }
        {
            let mut buffer = self.shared.buffer();
            buffer.parser.process(&frame.ansi);
            buffer.displayed = true;
        }
        self.shared.screen_changed.send_replace(());
        self.applied = Some((header.render_generation, header.frame_id));
        self.link
            .send(Body::FrameApplied(FrameApplied {
                render_generation: header.render_generation,
                frame_id: header.frame_id,
            }))
            .await?;
        Ok(())
    }
}
