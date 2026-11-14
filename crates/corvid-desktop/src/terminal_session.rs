use std::collections::HashMap;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
#[cfg(windows)]
use corvid_client::WslDistribution;
use corvid_client::{ControlEndpoint, InteractiveConnection, RemoteHost, SshAlias};
use corvid_companion::{Config, HostEndpoint};
use corvid_protocol::snapshot::SynchronizedSnapshot;
use corvid_protocol::wire::create_view::Target;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::layout_node_state::Body as Node;
use corvid_protocol::wire::snapshot_entity::Body as EntityBody;
use corvid_protocol::wire::{
    AgentInfo, Attach, AttachRole, Attached, ClaimWorkspaceWriter, ClientState, CloseView,
    CreateView, DisconnectWorkspaceClients, FocusViewAgent, FrameApplied, InputChunk,
    LaunchCommand, PasteBegin, PasteChunk, PasteEnd, ReadPanePreview, ReadTerminalHistory,
    ResizeViewport, RouteToken, SelectView, TerminalHistory, TerminalState, ViewSelection,
    Viewport, WorkspaceState,
};
use corvid_protocol::{
    CLAIM_WORKSPACE_CAPABILITY, CLOSE_VIEW_CAPABILITY, COMMAND_TIMEOUT, CREATE_VIEW_CAPABILITY,
    DEFAULT_SHELL_CAPABILITY, Frame, PANE_PREVIEW_CAPABILITY, ProtocolFault, RenderFrame,
    SESSION_CLIENTS_CAPABILITY, TERMINAL_HISTORY_CAPABILITY, TERMINAL_HISTORY_MAX_BYTES,
    TERMINAL_HISTORY_MAX_ROWS, decode_envelope,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, timeout};
use uuid::Uuid;
use vt100::{Parser, Screen};

use crate::daemon_prompt::{Prompt, Prompts};
use crate::{agent_deck, local_bootstrap, remote_bootstrap};

pub const MAX_INPUT: usize = 64 * 1024;
const INPUT_CHUNK: usize = 8192;
const PREVIEW_INTERVAL: Duration = Duration::from_millis(250);
type HistoryReply = std::result::Result<TerminalHistory, String>;

#[cfg(all(test, windows))]
mod wsl_test;

#[derive(Clone, PartialEq, Eq, Hash)]
pub enum Destination {
    Local,
    Ssh(SshAlias),
    #[cfg(windows)]
    Wsl(WslDistribution),
}

#[derive(Clone)]
pub enum AttachTarget {
    Default,
    Pane(ViewSelection, bool),
    Workspace {
        id: String,
        host_id: String,
        new_tab: Option<String>,
    },
    Takeover {
        view: ViewSelection,
        host_id: String,
        terminal_id: String,
    },
}

impl Destination {
    pub fn label(&self) -> &str {
        match self {
            Self::Local => "Local",
            Self::Ssh(alias) => alias.as_str(),
            #[cfg(windows)]
            Self::Wsl(distribution) => distribution.as_str(),
        }
    }

    fn remote(&self) -> Option<RemoteHost> {
        match self {
            Self::Local => None,
            Self::Ssh(alias) => Some(RemoteHost::Ssh(alias.clone())),
            #[cfg(windows)]
            Self::Wsl(distribution) => Some(RemoteHost::Wsl(distribution.clone())),
        }
    }
}

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
    pub clients: Vec<ClientState>,
}

impl Inventory {
    pub fn read(state: &SynchronizedSnapshot) -> Self {
        let mut inventory = Self::default();
        for entity in state.snapshot().entities() {
            match &entity.body {
                Some(EntityBody::Workspace(workspace)) => {
                    inventory.workspaces.push(workspace.clone())
                }
                Some(EntityBody::Terminal(terminal)) => inventory.terminals.push(terminal.clone()),
                Some(EntityBody::Agent(agent)) if agent.parent_run_id.is_none() => {
                    inventory.agents.extend(agent.summary.clone());
                }
                Some(EntityBody::Client(client)) => inventory.clients.push(client.clone()),
                _ => {}
            }
        }
        inventory
    }

    pub fn live(&self, selection: &ViewSelection) -> bool {
        self.terminals
            .iter()
            .any(|terminal| terminal.location.as_ref() == Some(selection))
    }

    pub fn has_writer(&self, selection: &ViewSelection) -> bool {
        self.clients
            .iter()
            .any(|client| client.active_selection() == Some(selection))
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
    pub naming_supported: bool,
    pub host_id: Option<String>,
    pub endpoint: Option<ControlEndpoint>,
    pub terminal_id: Option<String>,
    pub selected: Option<ViewSelection>,
    pub inventory: Option<Arc<Inventory>>,
    pub inventory_ready: bool,
    pub message: String,
    pub connected: bool,
    pub changing: bool,
    pub close_supported: bool,
    pub session_clients_supported: bool,
    pub closed_view: bool,
    pub deck_focus: u64,
}

impl Ui {
    pub fn can_write(&self) -> bool {
        self.connected
    }

    pub fn can_close(&self) -> bool {
        self.can_write() && self.close_supported && !self.changing
    }
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
    Rename(corvid_protocol::wire::RenameView),
    Input {
        revision: u64,
        bytes: Vec<u8>,
        paste: bool,
    },
    ReadHistory(u64, oneshot::Sender<HistoryReply>),
    DisconnectOthers(u64),
    Select(ViewSelection, bool),
    Create(Target),
    Close(CloseView),
    OpenAgentDeck(agent_deck::Settings),
}

struct Requests {
    actions: mpsc::Receiver<Action>,
    sizes: watch::Receiver<(u64, Grid)>,
    prompts: Prompts,
    host: Option<String>,
    terminal: Option<String>,
}

impl Session {
    pub fn start(
        destination: Destination,
        state_dir: PathBuf,
        target: AttachTarget,
        explicit: bool,
    ) -> Result<(Self, Job, mpsc::UnboundedReceiver<Prompt>)> {
        let grid = Grid::new(26, 112);
        let (screen_changed, _) = watch::channel(());
        let (ui_changed, _) = watch::channel(());
        let shared = Arc::new(Shared {
            buffer: Mutex::new(Buffer {
                ui: Ui {
                    naming_supported: false,
                    host_id: None,
                    endpoint: None,
                    terminal_id: None,
                    selected: None,
                    inventory: None,
                    inventory_ready: false,
                    message: match destination {
                        Destination::Local => "Opening the local terminal…".into(),
                        _ => "Connecting to Corvid…".into(),
                    },
                    connected: false,
                    changing: true,
                    close_supported: false,
                    session_clients_supported: false,
                    closed_view: false,
                    deck_focus: 0,
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
        let (prompt_sender, prompts) = mpsc::unbounded_channel();
        let (size, sizes) = watch::channel((0, grid));
        let requests = Requests {
            actions: incoming,
            sizes,
            prompts: Prompts(prompt_sender),
            host: None,
            terminal: None,
        };
        let (stop, mut stopped) = oneshot::channel();
        let state = Arc::clone(&shared);
        let thread = thread::Builder::new().name("corvid-desktop-client".into()).spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread().enable_all().build()
                .map_err(anyhow::Error::from)
                .and_then(|runtime| runtime.block_on(async {
                    let mut agent_deck = agent_deck::Job::default();
                    let result = tokio::select! {
                        result = run_reconnecting(destination, state_dir, target, explicit, requests, &state, &mut agent_deck) => result,
                        _ = &mut stopped => Ok("Detached from Corvid.".into()),
                    };
                    agent_deck.close().await;
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
            prompts,
        ))
    }

    pub fn ui(&self) -> Ui {
        self.shared.buffer().ui.clone()
    }

    pub fn ui_with_revision(&self) -> (Ui, u64) {
        let buffer = self.shared.buffer();
        (buffer.ui.clone(), buffer.revision)
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

    pub fn history(&self, pane: &str) -> Option<oneshot::Receiver<HistoryReply>> {
        let buffer = self.shared.buffer();
        if !buffer.ui.can_write() || !buffer.displayed || !in_pane(&buffer.ui, pane) {
            return None;
        }
        let (send, receive) = oneshot::channel();
        self.actions
            .try_send(Action::ReadHistory(buffer.revision, send))
            .ok()?;
        Some(receive)
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
        if !buffer.ui.can_write()
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

    pub fn select(&self, selection: ViewSelection, open: bool) {
        self.change(Action::Select(selection, open));
    }

    pub fn create(&self, target: Target) {
        self.change(Action::Create(target));
    }

    pub fn view_revision(&self) -> u64 {
        self.shared.buffer().revision
    }

    pub fn close(&self, target: CloseView) {
        self.change(Action::Close(target));
    }

    pub fn rename_tab(&self, name: String, revision: u64) {
        self.change(Action::Rename(corvid_protocol::wire::RenameView {
            expected_view_revision: Some(revision),
            pane: false,
            name,
        }));
    }

    pub fn disconnect_others(&self, revision: u64) {
        if self.ui().session_clients_supported {
            self.change(Action::DisconnectOthers(revision));
        }
    }

    pub fn open_agent_deck(&self, settings: agent_deck::Settings) {
        if self.shared.buffer().ui.connected {
            let _ = self.actions.try_send(Action::OpenAgentDeck(settings));
        }
    }

    fn change(&self, action: Action) {
        let mut buffer = self.shared.buffer();
        if !buffer.ui.can_write() || buffer.ui.changing {
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
    endpoint: ControlEndpoint,
    agent_deck: &'a mut agent_deck::Job,
    deck_requests: mpsc::Sender<agent_deck::FocusRequest>,
    deck_result: Option<oneshot::Sender<Result<(), String>>>,
    shared: &'a Shared,
    selected: ViewSelection,
    route: RouteToken,
    grid: Grid,
    applied: Option<(u64, u64)>,
    sequence: u64,
    pending: Option<Pending>,
    preview: Option<(String, Pending)>,
    history: Option<HistoryRequest>,
    preview_index: usize,
    next_preview: Instant,
}

struct HistoryRequest {
    id: String,
    revision: u64,
    deadline: Instant,
    reply: oneshot::Sender<HistoryReply>,
}

struct Pending {
    id: String,
    deadline: Instant,
    kind: Operation,
}

enum Operation {
    Rename,
    Claim(ViewSelection, bool, Option<String>),
    ClaimClose(CloseView),
    View,
    Close,
    Resize,
    Preview,
    DisconnectOthers,
}

async fn run_reconnecting(
    destination: Destination,
    state_dir: PathBuf,
    mut target: AttachTarget,
    mut explicit: bool,
    mut requests: Requests,
    shared: &Shared,
    agent_deck: &mut agent_deck::Job,
) -> Result<String> {
    let mut reconnecting = false;
    loop {
        let result = run(
            destination.clone(),
            state_dir.clone(),
            target.clone(),
            explicit,
            &mut requests,
            shared,
            agent_deck,
        )
        .await;
        let error = match result {
            Ok(message) => return Ok(message),
            Err(error) => error,
        };
        let ui = shared.buffer().ui.clone();
        let transient = error
            .downcast_ref::<ProtocolFault>()
            .is_none_or(|fault| matches!(fault.code.as_str(), "Offline" | "DaemonUnavailable"));
        if !transient || (!reconnecting && !ui.connected) {
            return Err(error);
        }
        let Some(selected) = ui.selected else {
            return Err(error);
        };
        target = AttachTarget::Pane(selected, false);
        explicit = false;
        reconnecting = true;
        requests.terminal = ui.terminal_id;
        shared.update_ui(|ui| {
            ui.connected = false;
            ui.inventory_ready = false;
            ui.changing = false;
            ui.message = "Reconnecting to the existing Corvid terminal…".into();
        });
        // Disable new input before draining requests for the retired writer.
        while requests.actions.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn run(
    destination: Destination,
    state_dir: PathBuf,
    target: AttachTarget,
    explicit: bool,
    Requests {
        actions,
        sizes,
        prompts,
        host,
        terminal,
    }: &mut Requests,
    shared: &Shared,
    agent_deck: &mut agent_deck::Job,
) -> Result<String> {
    let mut capabilities: Vec<_> = [
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
    if matches!(target, AttachTarget::Takeover { .. }) {
        capabilities.push(SESSION_CLIENTS_CAPABILITY.into());
        capabilities.push(corvid_protocol::PANE_WRITERS_CAPABILITY.into());
    }
    let known_host = match &target {
        AttachTarget::Workspace { host_id, .. } | AttachTarget::Takeover { host_id, .. } => {
            Some(host_id.as_str())
        }
        _ => None,
    };
    let progress = |message: &str| shared.update_ui(|ui| ui.message = message.into());
    let (endpoint, mut link) = if let Some(remote) = destination.remote() {
        let configured_host = Config::read_if_present(&state_dir)?.and_then(|config| {
            let matches = match (&config.endpoint, &remote) {
                (HostEndpoint::Ssh { alias }, RemoteHost::Ssh(target)) => alias == target.as_str(),
                (HostEndpoint::Wsl { distribution }, RemoteHost::Wsl(target)) => {
                    distribution == target.as_str()
                }
                _ => false,
            };
            matches.then(|| config.host_id.to_string())
        });
        let expected_host = host
            .as_deref()
            .or(configured_host.as_deref())
            .or(known_host);
        let (remote, link) = remote_bootstrap::connect(
            remote,
            capabilities.clone(),
            explicit,
            expected_host,
            prompts,
            progress,
        )
        .await?;
        ensure!(
            expected_host.is_none_or(|host| host == link.session().host_id),
            ProtocolFault::new(
                "HostMismatch",
                "connection points to a different Corvid host"
            )
        );
        (remote.endpoint(), link)
    } else {
        local_bootstrap::connect(capabilities.clone(), explicit, prompts, progress).await?
    };
    ensure!(
        host.as_deref()
            .or(known_host)
            .is_none_or(|host| host == link.session().host_id),
        ProtocolFault::new(
            "HostMismatch",
            "the connection points to a different Corvid host"
        )
    );
    *host = Some(link.session().host_id.clone());
    // Keep the verified endpoint available to Sessions even when attach is busy.
    shared.update_ui(|ui| {
        ui.host_id = host.clone();
        ui.endpoint = Some(endpoint.clone());
    });
    let grid = sizes.borrow().1;
    let resume = terminal.is_none();
    let attached = match &target {
        AttachTarget::Workspace {
            id,
            host_id,
            new_tab,
        } => {
            ensure!(
                link.session().host_id == *host_id,
                "The connection points to a different Corvid host"
            );
            let mut request = Attach {
                workspace_id: Some(id.clone()),
                create_tab: new_tab.clone(),
                launch: new_tab.as_ref().map(|_| LaunchCommand::default()),
                ..Default::default()
            };
            match attach(&mut link, grid, resume, request.clone()).await {
                Err(error)
                    if new_tab.is_none()
                        && error
                            .downcast_ref::<ProtocolFault>()
                            .is_some_and(|fault| fault.code == "NoTerminal") =>
                {
                    drop(link);
                    link = endpoint.interactive(capabilities).await?;
                    ensure!(
                        link.session().host_id == *host_id,
                        "The connection points to a different Corvid host"
                    );
                    request.launch = Some(LaunchCommand::default());
                    attach(&mut link, grid, resume, request).await?
                }
                result => result?,
            }
        }
        AttachTarget::Takeover {
            view,
            host_id,
            terminal_id,
        } => {
            ensure!(
                link.session().host_id == *host_id,
                "The connection points to a different Corvid host"
            );
            attach(
                &mut link,
                grid,
                resume,
                Attach {
                    workspace_id: Some(view.workspace_id.clone()),
                    tab_id: Some(view.tab_id.clone()),
                    pane_id: Some(view.pane_id.clone()),
                    take_over: true,
                    expected_terminal_id: Some(terminal_id.clone()),
                    ..Default::default()
                },
            )
            .await?
        }
        AttachTarget::Pane(view, open) => {
            attach(
                &mut link,
                grid,
                resume,
                Attach {
                    workspace_id: Some(view.workspace_id.clone()),
                    tab_id: Some(view.tab_id.clone()),
                    pane_id: Some(view.pane_id.clone()),
                    launch: open.then(LaunchCommand::default),
                    ..Default::default()
                },
            )
            .await?
        }
        AttachTarget::Default => {
            attach(
                &mut link,
                grid,
                resume,
                Attach {
                    terminal_capabilities: vec![DEFAULT_SHELL_CAPABILITY.into()],
                    launch: Some(LaunchCommand::default()),
                    ..Default::default()
                },
            )
            .await?
        }
    };
    let route = attached.route_token.context("missing input route")?;
    ensure!(
        terminal.as_ref().is_none_or(|id| *id == route.terminal_id),
        ProtocolFault::new(
            "TerminalChanged",
            "the previous terminal ended while the daemon was disconnected"
        )
    );
    let selected = attached.selection.context("missing attached view")?;
    validate_selection(&selected)?;
    ensure!(
        route.view_revision == 1
            && route.connection_generation > 0
            && route.connection_generation == attached.connection_generation
            && route.client_id == attached.client_id
            && valid_route(&route)
            && route.writer_lease_generation > 0
            && match &target {
                AttachTarget::Default => true,
                AttachTarget::Pane(view, _) | AttachTarget::Takeover { view, .. } =>
                    *view == selected,
                AttachTarget::Workspace { id, .. } => *id == selected.workspace_id,
            },
        "invalid initial daemon route"
    );

    let host = link.session().host_id.clone();
    let epoch = link.session().server_epoch.clone();
    let close_supported = link
        .session()
        .capabilities
        .iter()
        .any(|name| name == CLOSE_VIEW_CAPABILITY);
    let session_clients_supported = link
        .session()
        .capabilities
        .iter()
        .any(|name| name == SESSION_CLIENTS_CAPABILITY);
    shared.update_ui(|ui| {
        ui.close_supported = close_supported;
        ui.naming_supported = link
            .session()
            .capabilities
            .iter()
            .any(|name| name == corvid_protocol::VIEW_TITLES_CAPABILITY);
        ui.session_clients_supported = session_clients_supported;
    });
    let (deck_requests, mut deck_focus) = mpsc::channel(1);
    let mut client = Client {
        link,
        endpoint: endpoint.clone(),
        agent_deck,
        deck_requests,
        deck_result: None,
        shared,
        selected,
        route,
        grid,
        applied: None,
        sequence: 0,
        pending: None,
        preview: None,
        history: None,
        preview_index: 0,
        next_preview: Instant::now(),
    };
    client.publish_view();
    let inventory = watch_inventory(endpoint, host, epoch, shared);
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
            Some(request) = deck_focus.recv() => client.focus_from_deck(request).await?,
            result = client.agent_deck.tasks.join_next(), if !client.agent_deck.tasks.is_empty() => {
                match result {
                    Some(Ok(Err(error))) => shared.update_ui(|ui| ui.message = format!("{error:#}")),
                    Some(Err(_)) => shared.update_ui(|ui| ui.message = "Corvid Agent Deck stopped unexpectedly.".into()),
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

async fn attach(
    link: &mut InteractiveConnection,
    grid: Grid,
    resume: bool,
    mut request: Attach,
) -> Result<Attached> {
    let supports_resume = [
        corvid_protocol::PANE_WRITERS_CAPABILITY,
        SESSION_CLIENTS_CAPABILITY,
        corvid_protocol::RESUME_ATTACH_CAPABILITY,
    ]
    .iter()
    .all(|capability| {
        link.session()
            .capabilities
            .iter()
            .any(|name| name == capability)
    });
    let existing = request.create_workspace.is_none()
        && request.create_tab.is_none()
        && (request.launch.is_none()
            || request
                .terminal_capabilities
                .iter()
                .any(|name| name == DEFAULT_SHELL_CAPABILITY));
    request.take_over |= resume && supports_resume && existing;
    request.viewport = Some(grid.wire());
    request.terminal_capabilities.push("legacy_vt.v1".into());
    if request.create_tab.is_some()
        && link
            .session()
            .capabilities
            .iter()
            .any(|name| name == corvid_protocol::VIEW_TITLES_CAPABILITY)
    {
        request.create_tab = Some(String::new());
    }
    if link
        .session()
        .capabilities
        .iter()
        .any(|name| name == corvid_protocol::PANE_WRITERS_CAPABILITY)
    {
        request
            .terminal_capabilities
            .push(corvid_protocol::PANE_WRITERS_CAPABILITY.into());
    }
    if request.requested_role == AttachRole::Unspecified as i32 {
        request.requested_role = AttachRole::Writer as i32;
    }
    let id = link.send(Body::Attach(request)).await?;
    let reply = timeout(COMMAND_TIMEOUT, link.receive()).await??;
    let (reply_id, reply) = envelope(link, reply)?;
    ensure!(
        reply_id.as_deref() == Some(&id),
        "unexpected attach response ID"
    );
    match reply {
        Body::Attached(attached) => Ok(attached),
        Body::Error(error) => Err(ProtocolFault::from_wire(&error).into()),
        _ => bail!("unexpected attach response"),
    }
}

async fn watch_inventory(endpoint: ControlEndpoint, host: String, epoch: String, shared: &Shared) {
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
        let inventory = Inventory::read(watch.state());
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
    Uuid::parse_str(&route.client_id).is_ok() && Uuid::parse_str(&route.terminal_id).is_ok()
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
            buffer.ui.terminal_id = Some(self.route.terminal_id.clone());
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
        if let Some(history) = &self.history {
            ensure!(now < history.deadline, "daemon terminal history timed out");
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
            Action::Rename(target) => {
                self.request(Body::RenameView(target), Operation::Rename)
                    .await?;
            }
            Action::DisconnectOthers(revision) => {
                self.request(
                    Body::DisconnectWorkspaceClients(DisconnectWorkspaceClients {
                        expected_view_revision: Some(revision),
                    }),
                    Operation::DisconnectOthers,
                )
                .await?;
            }
            Action::ReadHistory(revision, reply) => self.read_history(revision, reply).await?,
            Action::OpenAgentDeck(settings) if self.agent_deck.tasks.is_empty() => {
                let host = self.link.session().host_id.clone();
                self.agent_deck.spawn(
                    settings,
                    self.endpoint.clone(),
                    host,
                    self.deck_requests.clone(),
                );
            }
            Action::OpenAgentDeck(_) => self.agent_deck.show(),
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
            Action::Select(selection, open) => self.begin_select(selection, open, None).await?,
            Action::Create(target) => self.create(target).await?,
            Action::Close(target) if target.workspace_id != self.selected.workspace_id => {
                self.request(
                    Body::ClaimWorkspaceWriter(ClaimWorkspaceWriter {
                        workspace_id: target.workspace_id.clone(),
                    }),
                    Operation::ClaimClose(target),
                )
                .await?;
            }
            Action::Close(target) => {
                self.request(Body::CloseView(target), Operation::Close)
                    .await?
            }
        }
        Ok(())
    }

    async fn read_history(
        &mut self,
        revision: u64,
        reply: oneshot::Sender<HistoryReply>,
    ) -> Result<()> {
        if reply.is_closed() {
            return Ok(());
        }
        let available = self
            .link
            .session()
            .capabilities
            .iter()
            .any(|name| name == TERMINAL_HISTORY_CAPABILITY);
        let error = if !available {
            Some("This daemon does not support terminal history.")
        } else if self.history.is_some() {
            Some("A terminal history request is already in progress.")
        } else if self.pending.is_some() || revision != self.route.view_revision {
            Some("The terminal view changed. Open history again.")
        } else {
            None
        };
        if let Some(error) = error {
            let _ = reply.send(Err(error.into()));
            return Ok(());
        }
        let id = self
            .link
            .send(Body::ReadTerminalHistory(ReadTerminalHistory {
                expected_view_revision: Some(revision),
            }))
            .await?;
        self.history = Some(HistoryRequest {
            id,
            revision,
            deadline: Instant::now() + COMMAND_TIMEOUT,
            reply,
        });
        Ok(())
    }

    async fn focus_from_deck(&mut self, request: agent_deck::FocusRequest) -> Result<()> {
        let run = request.run;
        let session = self.link.session();
        if run.host_id != session.host_id || run.remote_epoch != session.server_epoch {
            let _ = request.result.send(Err(
                "The agent belongs to a different daemon session.".into()
            ));
            return Ok(());
        }
        let selection = {
            let mut buffer = self.shared.buffer();
            if buffer.ui.changing || self.pending.is_some() {
                let _ = request
                    .result
                    .send(Err("Desktop is changing views. Try again.".into()));
                return Ok(());
            }
            let selection = buffer
                .ui
                .inventory
                .as_ref()
                .filter(|_| buffer.ui.inventory_ready)
                .and_then(|inventory| {
                    inventory.agents.iter().find(|agent| {
                        agent.agent_run_id == run.agent_run_id && agent.terminal_available
                    })
                })
                .and_then(|agent| agent.location.clone());
            let Some(selection) = selection else {
                let _ = request
                    .result
                    .send(Err("The agent pane is no longer available.".into()));
                return Ok(());
            };
            buffer.ui.changing = true;
            selection
        };
        self.shared.ui_changed.send_replace(());
        self.deck_result = Some(request.result);
        self.begin_select(selection, false, Some(run.agent_run_id))
            .await
    }

    fn finish_deck_focus(&mut self, result: Result<(), String>) {
        if let Some(reply) = self.deck_result.take() {
            if result.is_ok() {
                self.shared
                    .update_ui(|ui| ui.deck_focus = ui.deck_focus.wrapping_add(1));
            }
            let _ = reply.send(result);
        }
    }

    async fn begin_select(
        &mut self,
        selection: ViewSelection,
        open: bool,
        run: Option<String>,
    ) -> Result<()> {
        if selection.workspace_id != self.selected.workspace_id {
            let body = Body::ClaimWorkspaceWriter(ClaimWorkspaceWriter {
                workspace_id: selection.workspace_id.clone(),
            });
            self.request(body, Operation::Claim(selection, open, run))
                .await
        } else {
            self.select(selection, open, run).await
        }
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
        let target =
            if matches!(target, Target::TabLabel(_)) && self.shared.buffer().ui.naming_supported {
                Target::TabLabel(String::new())
            } else {
                target
            };
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
        let body = Body::ResizeViewport(ResizeViewport {
            client_id: self.route.client_id.clone(),
            connection_generation: self.route.connection_generation,
            viewport: Some(grid.wire()),
        });
        {
            let mut buffer = self.shared.buffer();
            if buffer.ui.changing {
                return Ok(());
            }
            buffer.ui.changing = true;
        }
        self.request(body, Operation::Resize).await
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
        if self
            .history
            .as_ref()
            .is_some_and(|history| id.as_deref() == Some(&history.id))
        {
            let pending = self.history.take().expect("matching history request");
            let result = match body {
                Body::TerminalHistory(history) if pending.revision == self.route.view_revision => {
                    validate_history(&history)?;
                    Ok(history)
                }
                Body::TerminalHistory(_) => {
                    Err("The terminal view changed. Open history again.".into())
                }
                Body::Error(error) => Err(ProtocolFault::from_wire(&error).to_string()),
                _ => bail!("unexpected terminal history reply"),
            };
            let _ = pending.reply.send(result);
            return Ok(None);
        }
        match body {
            Body::ViewRenamed(_) => {
                let pending = self.pending.take().context("unexpected rename result")?;
                ensure!(
                    id.as_deref() == Some(&pending.id) && matches!(pending.kind, Operation::Rename),
                    "rename result targets another request"
                );
                self.shared.update_ui(|ui| ui.changing = false);
            }
            Body::ViewChanged(view) => {
                let route = view.route_token.context("view update has no route")?;
                let selection = view.selection.context("view update has no selection")?;
                validate_selection(&selection)?;
                ensure!(
                    view.view_revision == route.view_revision
                        && valid_route(&route)
                        && route.writer_lease_generation > 0
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
                if pending {
                    self.finish_deck_focus(Ok(()));
                }
            }
            Body::WriterClaimed(claim) => {
                let pending = self.pending.take().context("unsolicited writer claim")?;
                if let Operation::ClaimClose(target) = pending.kind {
                    ensure!(
                        id.as_deref() == Some(&pending.id)
                            && claim.workspace_id == target.workspace_id
                            && claim.generation > 0,
                        "invalid writer claim"
                    );
                    self.request(Body::CloseView(target), Operation::Close)
                        .await?;
                    return Ok(None);
                }
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
                    self.finish_deck_focus(Err("The selected agent has moved or ended.".into()));
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
                let message = ProtocolFault::from_wire(&error).to_string();
                if matched {
                    self.finish_deck_focus(Err(message.clone()));
                }
                self.shared.update_ui(|ui| {
                    if matched {
                        ui.changing = false;
                    }
                    ui.message = message;
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
            Body::WorkspaceClientsDisconnected(reply) => {
                let pending = self
                    .pending
                    .take()
                    .context("unexpected disconnect result")?;
                ensure!(
                    id.as_deref() == Some(&pending.id)
                        && matches!(pending.kind, Operation::DisconnectOthers),
                    "disconnect result targets another request"
                );
                self.shared.update_ui(|ui| {
                    ui.changing = false;
                    ui.message = match reply.client_count {
                        0 => "No other clients in this session.".into(),
                        1 => "Disconnected one other client.".into(),
                        count => format!("Disconnected {count} other clients."),
                    };
                });
            }
            Body::Detach(_) => {
                if self.pending.as_ref().is_some_and(|pending| {
                    id.as_deref() == Some(&pending.id) && matches!(pending.kind, Operation::Close)
                }) {
                    self.shared.update_ui(|ui| {
                        ui.closed_view = true;
                        ui.inventory = None;
                        ui.inventory_ready = false;
                        ui.selected = None;
                    });
                    return Ok(Some("View closed.".into()));
                }
                return Ok(Some("Detached from Corvid.".into()));
            }
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
            // TODO: Isolate Desktop ANSI parsing from the UI lock
            // Publish a completed screen snapshot so large frames cannot block UI/input reads.
            // Issue URL: https://github.com/DosangGu/corvid/issues/31
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

fn validate_history(history: &TerminalHistory) -> Result<()> {
    ensure!(
        (1..=1024).contains(&history.columns)
            && history.lines.len() <= TERMINAL_HISTORY_MAX_ROWS
            && history
                .lines
                .iter()
                .map(|line| line.len() + 8)
                .sum::<usize>()
                <= TERMINAL_HISTORY_MAX_BYTES
            && history
                .lines
                .iter()
                .all(|line| !line.chars().any(char::is_control)),
        "invalid terminal history"
    );
    Ok(())
}
