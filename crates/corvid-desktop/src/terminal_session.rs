use std::collections::HashMap;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as blocking;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use corvid_async::time::{Instant, timeout};
use corvid_async::{mpsc, oneshot, watch};
#[cfg(windows)]
use corvid_client::WslDistribution;
use corvid_client::view_state::{
    Recorder, SavedView, Target as SavedTarget, ViewStore, workspace_preference_missed,
};
use corvid_client::{
    AttachRecovery, ControlEndpoint, InteractiveConnection, RemoteHost, SshAlias, SshRetry,
};
use corvid_companion::{Config, HostEndpoint};
use corvid_presentation::desktop_focus::{FocusFailure, FocusReply};
use corvid_protocol::ansi_screen::{Decoder, Screen};
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
    Viewport, WheelEncoding, WorkspaceState,
};
use corvid_protocol::{
    CLAIM_WORKSPACE_CAPABILITY, CLIPBOARD_WRITE_CAPABILITY, CLOSE_VIEW_CAPABILITY, COMMAND_TIMEOUT,
    CREATE_VIEW_CAPABILITY, DEFAULT_SHELL_CAPABILITY, Frame, GRACEFUL_DETACH_CAPABILITY,
    PANE_PREVIEW_CAPABILITY, ProtocolFault, RenderFrame, SESSION_CLIENTS_CAPABILITY,
    TERMINAL_HISTORY_CAPABILITY, TERMINAL_HISTORY_MAX_BYTES, TERMINAL_HISTORY_MAX_ROWS,
    WRITER_RESUME_CAPABILITY, decode_envelope,
};
use uuid::Uuid;

use crate::daemon_prompt::{Prompt, Prompts};
use crate::{agent_deck, local_bootstrap, remote_bootstrap};

pub const MAX_INPUT: usize = 64 * 1024;
const INPUT_CHUNK: usize = 8192;
const PREVIEW_INTERVAL: Duration = Duration::from_millis(250);
const DETACH_TIMEOUT: Duration = Duration::from_secs(2);
type HistoryReply = std::result::Result<TerminalHistory, String>;

mod inventory_watch;

use inventory_watch::InventoryWatch;

#[cfg(all(test, windows))]
mod wsl_test;

#[cfg(test)]
mod render_test;

#[cfg(test)]
mod restore_test;

#[derive(Clone, PartialEq, Eq, Hash)]
pub enum Destination {
    Local,
    Ssh(SshAlias),
    #[cfg(windows)]
    Wsl(WslDistribution),
}

#[derive(Clone)]
pub enum AttachTarget {
    Agent {
        endpoint: ControlEndpoint,
        run: corvid_protocol::wire::PresentedRunKey,
    },
    ResumeAgents {
        view: ViewSelection,
        host_id: String,
        pane_only: bool,
        automatic_epoch: Option<String>,
    },
    Default,
    FreshDefault,
    NewWorkspace {
        label: String,
        directory: Option<String>,
        endpoint: ControlEndpoint,
        host_id: String,
    },
    Restore(SavedView),
    Reconnect {
        view: ViewSelection,
        host_id: String,
        epoch: String,
        terminal_id: String,
        writer_resume_token: Option<Uuid>,
    },
    Pane(ViewSelection, bool),
    OpenPane {
        view: ViewSelection,
        host_id: String,
    },
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
    pub fn from_endpoint(endpoint: &ControlEndpoint) -> Result<Self> {
        Ok(match endpoint {
            ControlEndpoint::Local(_) => Self::Local,
            ControlEndpoint::Ssh(alias) => Self::Ssh(SshAlias::new(alias.as_str().to_owned())?),
            #[cfg(windows)]
            ControlEndpoint::Wsl(distribution) => {
                Self::Wsl(WslDistribution::new(distribution.as_str().to_owned())?)
            }
            #[cfg(not(windows))]
            ControlEndpoint::Wsl(_) => bail!("WSL requires Windows"),
        })
    }

    pub fn saved_target(&self) -> SavedTarget {
        match self {
            Self::Local => SavedTarget::Local,
            Self::Ssh(alias) => SavedTarget::Ssh {
                alias: alias.as_str().to_owned(),
            },
            #[cfg(windows)]
            Self::Wsl(distribution) => SavedTarget::Wsl {
                distribution: distribution.as_str().to_owned(),
            },
        }
    }

    pub fn from_saved(target: SavedTarget) -> Result<Self> {
        match target {
            SavedTarget::Local => Ok(Self::Local),
            SavedTarget::Ssh { alias } => Ok(Self::Ssh(SshAlias::new(alias)?)),
            #[cfg(windows)]
            SavedTarget::Wsl { distribution } => Ok(Self::Wsl(WslDistribution::new(distribution)?)),
            _ => bail!("saved connection is unavailable on this platform"),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Local => "Local",
            Self::Ssh(alias) => alias.as_str(),
            #[cfg(windows)]
            Self::Wsl(distribution) => distribution.as_str(),
        }
    }

    pub(crate) fn remote(&self) -> Option<RemoteHost> {
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

    fn live_preview_panes(&self, selected: &ViewSelection, tab_id: &str) -> Vec<String> {
        let Some(tab) = self
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == selected.workspace_id)
            .and_then(|workspace| workspace.tabs.iter().find(|tab| tab.tab_id == tab_id))
        else {
            return Vec::new();
        };
        tab.nodes
            .iter()
            .filter_map(|node| match node.body.as_ref() {
                Some(Node::PaneId(pane))
                    if *pane != selected.pane_id
                        && self.live(&ViewSelection {
                            tab_id: tab_id.to_owned(),
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
    pub restore_supported: bool,
    pub recovery_supported: bool,
    pub restore_results: Vec<corvid_protocol::wire::PaneRestoreResult>,
    pub restore_unavailable: bool,
    pub naming_supported: bool,
    pub split_target_supported: bool,
    pub host_id: Option<String>,
    pub epoch: Option<String>,
    pub endpoint: Option<ControlEndpoint>,
    pub terminal_id: Option<String>,
    pub selected: Option<ViewSelection>,
    pub inventory: Option<Arc<Inventory>>,
    pub inventory_ready: bool,
    pub message: String,
    pub connected: bool,
    pub changing: bool,
    pub close_supported: bool,
    pub tab_preview_supported: bool,
    pub session_clients_supported: bool,
    pub closed_view: bool,
    pub ssh_offline: bool,
    pub reconnect_target: Option<AttachTarget>,
    pub inventory_offline: bool,
}

impl Ui {
    pub fn can_write(&self) -> bool {
        self.connected
    }

    pub fn can_close(&self) -> bool {
        self.can_write() && self.close_supported && !self.changing
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WheelTarget {
    Scrollback,
    LegacyScrollback,
    Mouse(WheelEncoding),
    Cursor { application: bool },
}

#[derive(Clone)]
struct Snapshot {
    ui: Arc<Ui>,
    screen: Option<Arc<Screen>>,
    wheel_encoding: WheelEncoding,
    alternate_screen: Option<bool>,
    view_epoch: u64,
    revision: u64,
    previews: Arc<HashMap<String, Arc<Screen>>>,
}

impl Snapshot {
    fn wheel_target(&self, pane: &str) -> Option<WheelTarget> {
        let screen = self.screen.as_ref()?;
        if !self.ui.can_write() || self.ui.changing || !in_pane(&self.ui, pane) {
            return None;
        }
        Some(if self.wheel_encoding != WheelEncoding::Disabled {
            WheelTarget::Mouse(self.wheel_encoding)
        } else {
            match self.alternate_screen {
                Some(true) => WheelTarget::Cursor {
                    application: screen.application_cursor,
                },
                Some(false) => WheelTarget::Scrollback,
                None => WheelTarget::LegacyScrollback,
            }
        })
    }
}

pub struct DeckActivation {
    pub view_epoch: u64,
    pub revision: u64,
    pub reply: FocusReply,
}

#[derive(Clone, Copy)]
enum Changed {
    Ui,
    Screen,
    View,
}

type StateEdit = Box<dyn FnOnce(&mut Snapshot) -> Changed + Send>;

pub(super) struct ClipboardWrite {
    view_epoch: u64,
    text: String,
}

struct Shared {
    edits: mpsc::Sender<(StateEdit, blocking::SyncSender<()>)>,
    snapshots: watch::Receiver<Arc<Snapshot>>,
    screen_changed: watch::Sender<()>,
    view_changed: watch::Sender<()>,
    preview_tab: watch::Sender<Option<String>>,
    clipboard: watch::Sender<Option<ClipboardWrite>>,
    ui_changed: watch::Sender<()>,
    ssh_control_reconnect: watch::Sender<()>,
    view_change: Arc<AtomicBool>,
}

impl Shared {
    fn new(destination: &Destination, runtime: &corvid_async::task::SharedWorker) -> Self {
        let mut state = Snapshot {
            ui: Arc::new(Ui {
                restore_supported: false,
                recovery_supported: false,
                restore_results: Vec::new(),
                restore_unavailable: false,
                naming_supported: false,
                split_target_supported: false,
                host_id: None,
                epoch: None,
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
                tab_preview_supported: false,
                session_clients_supported: false,
                closed_view: false,
                ssh_offline: false,
                reconnect_target: None,
                inventory_offline: false,
            }),
            screen: None,
            wheel_encoding: WheelEncoding::Disabled,
            alternate_screen: None,
            view_epoch: 0,
            revision: 0,
            previews: Arc::new(HashMap::new()),
        };
        let (snapshots, latest) = watch::channel(Arc::new(state.clone()));
        let (edits, mut incoming) = mpsc::channel::<(StateEdit, blocking::SyncSender<()>)>(2);
        let (screen_changed, _) = watch::channel(());
        let (view_changed, _) = watch::channel(());
        let (ui_changed, _) = watch::channel(());
        let screen = screen_changed.clone();
        let view = view_changed.clone();
        let ui = ui_changed.clone();
        runtime.spawn(async move {
            while let Some((edit, committed)) = incoming.recv().await {
                let changed = edit(&mut state);
                snapshots.send_replace(Arc::new(state.clone()));
                if matches!(changed, Changed::Screen | Changed::View) {
                    screen.send_replace(());
                }
                if matches!(changed, Changed::Ui | Changed::View) {
                    ui.send_replace(());
                }
                if matches!(changed, Changed::View) {
                    view.send_replace(());
                }
                let _ = committed.send(());
            }
        });
        Self {
            edits,
            snapshots: latest,
            screen_changed,
            view_changed,
            preview_tab: watch::channel(None).0,
            clipboard: watch::channel(None).0,
            ui_changed,
            ssh_control_reconnect: watch::channel(()).0,
            view_change: Arc::new(AtomicBool::new(false)),
        }
    }

    fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&self.snapshots.borrow())
    }

    // The Desktop client and inventory each have a dedicated caller thread and
    // at most one outstanding edit. GPUI only reads the published snapshot.
    fn edit(&self, edit: impl FnOnce(&mut Snapshot) -> Changed + Send + 'static) {
        let (committed, confirmation) = blocking::sync_channel(1);
        self.edits
            .try_send((Box::new(edit), committed))
            .unwrap_or_else(|_| panic!("Desktop state actor unavailable"));
        confirmation.recv().expect("Desktop state actor stopped");
    }

    fn update_ui(&self, edit: impl FnOnce(&mut Ui) + Send + 'static) {
        self.edit(move |state| {
            edit(Arc::make_mut(&mut state.ui));
            Changed::Ui
        });
    }

    fn reserve_view_change(&self) -> Option<ViewChange> {
        self.view_change
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(ViewChange {
            busy: Arc::clone(&self.view_change),
            changed: self.ui_changed.clone(),
        })
    }

    fn apply_ansi(
        &self,
        parser: &mut Decoder,
        ansi: &[u8],
        wheel_encoding: WheelEncoding,
        alternate_screen: Option<bool>,
    ) {
        self.apply_frame(wheel_encoding, alternate_screen, || {
            parser.process(ansi);
            parser.screen().clone()
        });
    }

    fn apply_frame(
        &self,
        wheel_encoding: WheelEncoding,
        alternate_screen: Option<bool>,
        decode: impl FnOnce() -> Screen,
    ) {
        // Decode on the connection worker before editing the published state.
        let screen = Arc::new(decode());
        self.edit(move |state| {
            state.wheel_encoding = wheel_encoding;
            state.alternate_screen = alternate_screen;
            state.screen = Some(screen);
            Changed::Screen
        });
    }
}

struct ViewChange {
    busy: Arc<AtomicBool>,
    changed: watch::Sender<()>,
}

impl Drop for ViewChange {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
        self.changed.send_replace(());
    }
}

pub type Connection = (
    Session,
    Job,
    mpsc::UnboundedReceiver<Prompt>,
    mpsc::Receiver<DeckActivation>,
);

#[derive(Clone)]
pub struct Session {
    shared: Arc<Shared>,
    actions: mpsc::Sender<Action>,
    size: watch::Sender<(u64, u64, Grid)>,
}

// The reader and SSH relay tasks must outlive any one attached session.
pub(crate) fn session_runtime() -> Result<&'static corvid_async::task::SharedWorker> {
    static RUNTIME: std::sync::OnceLock<std::io::Result<corvid_async::task::SharedWorker>> =
        std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| corvid_async::task::SharedWorker::new("corvid-desktop-shared"))
        .as_ref()
        .map_err(|error| anyhow::anyhow!("Could not start Desktop connection runtime: {error}"))
}

type ParkedLink = (Destination, ControlEndpoint, InteractiveConnection);

enum ParkedCommand {
    Insert(Box<ParkedLink>),
    Take {
        destination: Destination,
        host: Option<String>,
        epoch: Option<String>,
        reply: oneshot::Sender<Option<ParkedLink>>,
    },
    Clear(oneshot::Sender<()>),
}

#[derive(Clone, Default)]
pub struct ParkedConnections(Arc<std::sync::OnceLock<mpsc::Sender<ParkedCommand>>>);

impl ParkedConnections {
    fn start(&self, runtime: &corvid_async::task::SharedWorker) {
        self.0.get_or_init(|| {
            let (sender, mut incoming) = mpsc::channel(8);
            runtime.spawn(async move {
                let mut links: Vec<ParkedLink> = Vec::new();
                while let Some(command) = incoming.recv().await {
                    match command {
                        ParkedCommand::Insert(link) => {
                            links.retain(|(destination, _, _)| *destination != link.0);
                            if links.len() == 8 {
                                links.remove(0);
                            }
                            links.push(*link);
                        }
                        ParkedCommand::Take {
                            destination,
                            host,
                            epoch,
                            reply,
                        } => {
                            let index = links.iter().position(|(key, _, link)| {
                                *key == destination
                                    && host
                                        .as_deref()
                                        .is_none_or(|host| host == link.session().host_id)
                                    && epoch
                                        .as_deref()
                                        .is_none_or(|epoch| epoch == link.session().server_epoch)
                            });
                            let _ = reply.send(index.map(|index| links.remove(index)));
                        }
                        ParkedCommand::Clear(reply) => {
                            links.clear();
                            let _ = reply.send(());
                        }
                    }
                }
            });
            sender
        });
    }

    async fn insert(
        &self,
        destination: Destination,
        endpoint: ControlEndpoint,
        link: InteractiveConnection,
    ) {
        if let Some(sender) = self.0.get() {
            let _ = sender
                .send(ParkedCommand::Insert(Box::new((
                    destination,
                    endpoint,
                    link,
                ))))
                .await;
        }
    }

    async fn take(
        &self,
        destination: &Destination,
        host: Option<&str>,
        epoch: Option<&str>,
    ) -> Option<(ControlEndpoint, InteractiveConnection)> {
        let (reply, result) = oneshot::channel();
        self.0
            .get()?
            .send(ParkedCommand::Take {
                destination: destination.clone(),
                host: host.map(str::to_owned),
                epoch: epoch.map(str::to_owned),
                reply,
            })
            .await
            .ok()?;
        let (_, endpoint, mut link) = result.await.ok()??;
        let alive = timeout(Duration::from_millis(750), async {
            let id = link
                .send(Body::Ping(corvid_protocol::wire::Ping {}))
                .await?;
            let frame = link.receive().await?;
            let (reply_id, body) = envelope(&link, frame)?;
            Ok::<_, anyhow::Error>(reply_id == Some(id) && matches!(body, Body::Pong(_)))
        })
        .await;
        matches!(alive, Ok(Ok(true))).then_some((endpoint, link))
    }

    pub fn clear(&self) {
        if let Some(sender) = self.0.get() {
            let (reply, result) = oneshot::channel();
            if sender.send_blocking(ParkedCommand::Clear(reply)).is_ok() {
                let _ = result.blocking_recv();
            }
        }
    }
}

pub struct Job {
    stop: Option<oneshot::Sender<bool>>,
    thread: thread::JoinHandle<()>,
}

impl Job {
    #[cfg(test)]
    pub(crate) fn delayed_shutdown() -> (Self, blocking::Receiver<bool>, blocking::Sender<()>) {
        let (stop, stopped) = oneshot::channel();
        let (entered, stopping) = blocking::channel();
        let (release, resume) = blocking::channel();
        let thread = thread::spawn(move || {
            let park = stopped.blocking_recv().unwrap();
            entered.send(park).unwrap();
            resume.recv_timeout(Duration::from_secs(10)).unwrap();
        });
        (
            Self {
                stop: Some(stop),
                thread,
            },
            stopping,
            release,
        )
    }

    pub fn stop(&mut self, park: bool) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(park);
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
        view_epoch: u64,
        revision: u64,
        bytes: Vec<u8>,
        paste: bool,
    },
    Change {
        action: Box<Action>,
        view_epoch: u64,
        reservation: ViewChange,
    },
    ReadHistory(HistoryRead),
    DisconnectOthers(u64),
    Select(ViewSelection, bool),
    Create(Target),
    Close(CloseView),
    OpenAgentDeck(agent_deck::Settings),
    FocusAgent(agent_deck::FocusRequest),
    CommitNavigation,
}

struct Requests {
    monitor: agent_deck::Settings,
    recorder: Option<Recorder>,
    view_store: ViewStore,
    actions: mpsc::Receiver<Action>,
    sizes: watch::Receiver<(u64, u64, Grid)>,
    activation: mpsc::Sender<DeckActivation>,
    prompts: Prompts,
    host: Option<String>,
    terminal: Option<String>,
    writer_resume_token: Option<Uuid>,
    closing: oneshot::Receiver<bool>,
    parked: ParkedConnections,
    attached: Option<oneshot::Sender<()>>,
    wake_monitor: bool,
}

impl Session {
    pub fn start(
        destination: Destination,
        settings: impl Into<agent_deck::Settings>,
        store: ViewStore,
        target: AttachTarget,
        explicit: bool,
        parked: ParkedConnections,
    ) -> Result<Connection> {
        let runtime = session_runtime()?;
        parked.start(runtime);
        let settings = settings.into();
        let state_dir = settings.state.clone();
        let grid = Grid::new(26, 112);
        let shared = Arc::new(Shared::new(&destination, runtime));
        let (actions, incoming) = mpsc::channel(32);
        let (prompt_sender, prompts) = mpsc::unbounded_channel();
        let (size, sizes) = watch::channel((0, 0, grid));
        let (activation, activations) = mpsc::channel(1);
        let (close, closing) = oneshot::channel();
        let (stop, mut stopped) = oneshot::channel();
        let state = Arc::clone(&shared);
        let ssh = matches!(destination, Destination::Ssh(_));
        let thread = corvid_async::task::worker("corvid-desktop-client", move |runtime| {
            let target = if matches!(target, AttachTarget::Default) {
                store
                    .for_target(&destination.saved_target())
                    .map(AttachTarget::Restore)
                    .unwrap_or(target)
            } else {
                target
            };
            let restoring = matches!(target, AttachTarget::Restore(_));
            let requests = Requests {
                monitor: settings,
                recorder: match store.recorder() {
                    Ok(recorder) => Some(recorder),
                    Err(error) => {
                        eprintln!("corvid: could not save Desktop views: {error:#}");
                        None
                    }
                },
                view_store: store,
                actions: incoming,
                sizes,
                activation,
                prompts: Prompts(prompt_sender),
                host: None,
                terminal: None,
                writer_resume_token: None,
                closing,
                parked,
                attached: None,
                wake_monitor: explicit && ssh,
            };
            let result = runtime.block_on(async {
                let mut agent_deck = agent_deck::Job::default();
                let result = {
                    let client = run_reconnecting(
                        destination,
                        state_dir,
                        target,
                        explicit,
                        requests,
                        &state,
                        &mut agent_deck,
                    );
                    corvid_async::pin!(client);
                    corvid_async::select! {
                        result = &mut client => result,
                        park = &mut stopped => {
                            let _ = close.send(park.unwrap_or(false));
                            if timeout(DETACH_TIMEOUT, &mut client).await.is_err() {
                                eprintln!("Timed out waiting for Desktop detach");
                            }
                            Ok("Detached from Corvid.".into())
                        }
                    }
                };
                agent_deck.close().await;
                result
            });
            state.update_ui(move |ui| {
                ui.ssh_offline = ssh
                    && result.as_ref().is_err_and(|error| {
                        SshRetry::is_transient(error)
                            || error.downcast_ref::<ProtocolFault>().is_some_and(|fault| {
                                matches!(
                                    fault.code.as_str(),
                                    "AuthenticationRequired" | "HostKeyRequired" | "HostMismatch"
                                )
                            })
                    });
                ui.restore_unavailable = restoring
                    && result
                        .as_ref()
                        .err()
                        .and_then(|error| error.downcast_ref::<ProtocolFault>())
                        .is_some_and(|fault| {
                            matches!(
                                fault.code.as_str(),
                                "NoTerminal"
                                    | "TargetGone"
                                    | "SavedWorkspaceGone"
                                    | "WorkspaceBusy"
                            )
                        });
                ui.connected = false;
                ui.inventory_ready = false;
                ui.inventory_offline = false;
                ui.changing = false;
                ui.message = match result {
                    Ok(message) => message,
                    Err(error) if ui.ssh_offline => error
                        .downcast_ref::<ProtocolFault>()
                        .map_or_else(|| format!("{error:#}"), |fault| fault.message.clone()),
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
            activations,
        ))
    }

    pub fn ui(&self) -> Ui {
        self.ui_with_revision().0
    }

    pub fn reconnect_ssh_controls(&self) {
        self.shared.ssh_control_reconnect.send_replace(());
    }

    pub fn ui_with_revision(&self) -> (Ui, u64) {
        let snapshot = self.shared.snapshot();
        let mut ui = (*snapshot.ui).clone();
        ui.changing |= self.shared.view_change.load(Ordering::Acquire);
        (ui, snapshot.revision)
    }

    pub fn matches_view(&self, view_epoch: u64, revision: u64) -> bool {
        let snapshot = self.shared.snapshot();
        snapshot.ui.connected && view_epoch == snapshot.view_epoch && revision == snapshot.revision
    }

    pub fn changes(&self) -> watch::Receiver<()> {
        self.shared.ui_changed.subscribe()
    }

    pub fn view_changes(&self) -> watch::Receiver<()> {
        self.shared.view_changed.subscribe()
    }

    pub(super) fn clipboard_changes(&self) -> watch::Receiver<Option<ClipboardWrite>> {
        self.shared.clipboard.subscribe()
    }

    pub(super) fn take_clipboard(&self, pane: &str) -> Option<String> {
        let snapshot = self.shared.snapshot();
        if !in_pane(&snapshot.ui, pane) {
            return None;
        }
        let mut clipboard = None;
        self.shared.clipboard.send_if_modified(|pending| {
            clipboard = pending.take();
            false
        });
        let clipboard = clipboard?;
        let snapshot = self.shared.snapshot();
        (snapshot.view_epoch == clipboard.view_epoch
            && snapshot.ui.can_write()
            && in_pane(&snapshot.ui, pane)
            && !self.changing(&snapshot))
        .then_some(clipboard.text)
    }

    pub fn screen_changes(&self) -> watch::Receiver<()> {
        self.shared.screen_changed.subscribe()
    }

    pub fn screen<R>(&self, pane: &str, read: impl FnOnce(&Screen) -> R) -> Option<R> {
        let screen = self.screen_snapshot(pane)?;
        Some(read(&screen))
    }

    pub fn screen_snapshot(&self, pane: &str) -> Option<Arc<Screen>> {
        let snapshot = self.shared.snapshot();
        if !in_pane(&snapshot.ui, pane) {
            return None;
        }
        snapshot.screen.clone()
    }

    pub fn set_preview_tab(&self, tab: Option<&str>) {
        self.shared.preview_tab.send_if_modified(|current| {
            if current.as_deref() == tab {
                return false;
            }
            *current = tab.map(str::to_owned);
            true
        });
    }

    pub fn preview<R>(&self, pane: &str, read: impl FnOnce(&Screen) -> R) -> Option<R> {
        let screen = self.shared.snapshot().previews.get(pane)?.clone();
        Some(read(&screen))
    }

    pub fn history(&self, pane: &str, shell_only: bool) -> Option<oneshot::Receiver<HistoryReply>> {
        let snapshot = self.shared.snapshot();
        if !snapshot.ui.can_write()
            || self.changing(&snapshot)
            || snapshot.screen.is_none()
            || !in_pane(&snapshot.ui, pane)
        {
            return None;
        }
        let (send, receive) = oneshot::channel();
        self.actions
            .try_send(Action::ReadHistory(HistoryRead {
                view_epoch: snapshot.view_epoch,
                revision: snapshot.revision,
                shell_only,
                reply: send,
            }))
            .ok()?;
        Some(receive)
    }

    pub fn resize(&self, pane: &str, grid: Grid) {
        let snapshot = self.shared.snapshot();
        if !in_pane(&snapshot.ui, pane) {
            return;
        }
        self.size.send_if_modified(|last| {
            let desired = (snapshot.view_epoch, snapshot.revision, grid);
            let changed = *last != desired;
            *last = desired;
            changed
        });
    }

    pub fn wheel_target(&self, pane: &str) -> Option<(u64, WheelTarget)> {
        let snapshot = self.shared.snapshot();
        if self.changing(&snapshot) {
            return None;
        }
        Some((snapshot.view_epoch, snapshot.wheel_target(pane)?))
    }

    pub fn send_wheel(
        &self,
        pane: &str,
        route: (u64, WheelTarget),
        cell: (u16, u16),
        rows: isize,
        modifiers: u8,
    ) {
        let snapshot = self.shared.snapshot();
        if snapshot.view_epoch != route.0 || snapshot.wheel_target(pane) != Some(route.1) {
            return;
        }
        let bytes = match route.1 {
            WheelTarget::Mouse(encoding) => wheel_report(encoding, cell, rows, modifiers),
            WheelTarget::Cursor { application } => {
                let prefix = if application { b'O' } else { b'[' };
                let key = if rows < 0 { b'A' } else { b'B' };
                Some([0x1b, prefix, key].repeat(rows.unsigned_abs().min(32)))
            }
            _ => None,
        };
        if let Some(bytes) = bytes {
            self.queue_input(&snapshot, pane, bytes, false);
        }
    }

    pub fn send_pane_click(&self, pane: &str, (row, col): (u16, u16)) -> bool {
        let snapshot = self.shared.snapshot();
        let Some(WheelTarget::Mouse(encoding)) = snapshot.wheel_target(pane) else {
            return false;
        };
        let x = u32::from(col) + 1;
        let y = u32::from(row) + 1;
        let click = match encoding {
            WheelEncoding::Sgr => format!("\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m").into_bytes(),
            WheelEncoding::Legacy if x <= 223 && y <= 223 => {
                let x = (x + 32) as u8;
                let y = (y + 32) as u8;
                vec![27, b'[', b'M', 32, x, y, 27, b'[', b'M', 35, x, y]
            }
            WheelEncoding::Utf8 if x <= 2015 && y <= 2015 => {
                let x = char::from_u32(x + 32).expect("bounded mouse column");
                let y = char::from_u32(y + 32).expect("bounded mouse row");
                format!("\x1b[M {x}{y}\x1b[M#{x}{y}").into_bytes()
            }
            _ => return false,
        };
        self.queue_input(&snapshot, pane, click, false)
    }

    pub fn send(&self, pane: &str, bytes: Vec<u8>, paste: bool) -> bool {
        self.queue_input(&self.shared.snapshot(), pane, bytes, paste)
    }

    fn queue_input(&self, snapshot: &Snapshot, pane: &str, bytes: Vec<u8>, paste: bool) -> bool {
        if !snapshot.ui.can_write()
            || self.changing(snapshot)
            || snapshot.screen.is_none()
            || !in_pane(&snapshot.ui, pane)
            || bytes.is_empty()
            || bytes.len() > MAX_INPUT
        {
            return false;
        }
        self.actions
            .try_send(Action::Input {
                view_epoch: snapshot.view_epoch,
                revision: snapshot.revision,
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
        self.shared.snapshot().revision
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

    pub fn commit_navigation(&self) {
        let _ = self.actions.try_send(Action::CommitNavigation);
    }

    pub fn focus_agent(&self, request: agent_deck::FocusRequest) {
        if let Err(error) = self.actions.try_send(Action::FocusAgent(request))
            && let Action::FocusAgent(request) = error.into_inner()
        {
            let _ = request
                .result
                .send(Err("Desktop is busy. Try again.".into()));
        }
    }

    pub fn open_agent_deck(&self, settings: agent_deck::Settings) {
        if self.shared.snapshot().ui.connected {
            let _ = self.actions.try_send(Action::OpenAgentDeck(settings));
        }
    }

    fn changing(&self, snapshot: &Snapshot) -> bool {
        snapshot.ui.changing || self.shared.view_change.load(Ordering::Acquire)
    }

    fn change(&self, action: Action) {
        let snapshot = self.shared.snapshot();
        if !snapshot.ui.can_write() || self.changing(&snapshot) {
            return;
        }
        let Some(reservation) = self.shared.reserve_view_change() else {
            return;
        };
        if self
            .actions
            .try_send(Action::Change {
                action: Box::new(action),
                view_epoch: snapshot.view_epoch,
                reservation,
            })
            .is_ok()
        {
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
    recorder: Option<&'a Recorder>,
    record_navigation: bool,
    saved_target: SavedTarget,
    link: InteractiveConnection,
    endpoint: ControlEndpoint,
    agent_deck: &'a mut agent_deck::Job,
    deck_requests: mpsc::Sender<agent_deck::FocusRequest>,
    deck_result: Option<FocusReply>,
    shared: &'a Shared,
    selected: ViewSelection,
    route: RouteToken,
    grid: Grid,
    parser: Decoder,
    applied: Option<(u64, u64)>,
    sequence: u64,
    pending: Option<Pending>,
    view_change: Option<ViewChange>,
    activation: mpsc::Sender<DeckActivation>,
    preview: Option<(String, Pending)>,
    history: Option<HistoryRequest>,
    queued_history: Option<HistoryRead>,
    next_history: Instant,
    preview_index: usize,
    next_preview: Instant,
}

struct HistoryRead {
    view_epoch: u64,
    revision: u64,
    shell_only: bool,
    reply: oneshot::Sender<HistoryReply>,
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
    shared: &Arc<Shared>,
    agent_deck: &mut agent_deck::Job,
) -> Result<String> {
    let ssh = matches!(destination, Destination::Ssh(_));
    let mut reconnecting = matches!(target, AttachTarget::Reconnect { .. });
    let mut retry = reconnecting.then(SshRetry::default);
    if reconnecting {
        let retry_target = target.clone();
        shared.update_ui(move |ui| ui.reconnect_target = Some(retry_target));
    }
    loop {
        let result = {
            let (attached, mut ready) = oneshot::channel();
            requests.attached = Some(attached);
            let attempt = run(
                destination.clone(),
                state_dir.clone(),
                target.clone(),
                explicit,
                &mut requests,
                shared,
                agent_deck,
            );
            corvid_async::pin!(attempt);
            if let Some(retry) = &retry {
                corvid_async::select! {
                    biased;
                    result = &mut attempt => result,
                    _ = &mut ready => attempt.await,
                    _ = corvid_async::time::sleep_until(retry.deadline()) => {
                        Err(SshRetry::exhausted().into())
                    }
                }
            } else {
                attempt.await
            }
        };
        let error = match result {
            Ok(message) => return Ok(message),
            Err(error) => error,
        };
        if let Some(offer) = error.downcast_ref::<AttachRecovery>() {
            let ui = &shared.snapshot().ui;
            let endpoint = ui.endpoint.as_ref().context("missing recovery endpoint")?;
            let host = ui.host_id.as_deref().context("missing recovery host")?;
            let epoch = ui.epoch.as_deref().context("missing recovery epoch")?;
            if offer.should_resume() {
                target = AttachTarget::ResumeAgents {
                    view: offer.view.clone(),
                    host_id: host.to_owned(),
                    pane_only: true,
                    automatic_epoch: Some(epoch.to_owned()),
                };
                explicit = false;
                requests.terminal = None;
                shared.update_ui(|ui| {
                    ui.connected = false;
                    ui.changing = true;
                    ui.message = "Restoring saved agent conversation…".into();
                });
                // A restored agent starts on a new route. Discard requests for the retired writer.
                while requests.actions.try_recv().is_ok() {}
                continue;
            }
            let snapshot = match &retry {
                Some(retry) => corvid_async::time::timeout_at(
                    retry.deadline(),
                    endpoint.view_snapshot(host, epoch),
                )
                .await
                .map_err(|_| SshRetry::exhausted())??,
                None => endpoint.view_snapshot(host, epoch).await?,
            };
            let inventory = Arc::new(Inventory::read(&snapshot));
            let selected = offer.view.clone();
            let restore_results = vec![offer.pane.clone()];
            shared.update_ui(move |ui| {
                ui.inventory = Some(inventory);
                ui.selected = Some(selected);
                ui.restore_results = restore_results;
            });
            return Ok(offer.to_string());
        }
        let snapshot = shared.snapshot();
        let ui = &snapshot.ui;
        let transient = if ssh {
            SshRetry::is_transient(&error)
        } else {
            error
                .downcast_ref::<ProtocolFault>()
                .is_none_or(|fault| matches!(fault.code.as_str(), "Offline" | "DaemonUnavailable"))
        };
        if !transient || (!reconnecting && !ui.connected) {
            return Err(error);
        }
        let Some(selected) = ui.selected.clone() else {
            return Err(error);
        };
        if ssh {
            if ui.connected {
                retry = Some(SshRetry::default());
                target = AttachTarget::Reconnect {
                    view: selected,
                    host_id: ui.host_id.clone().context("missing reconnect host")?,
                    epoch: ui.epoch.clone().context("missing reconnect epoch")?,
                    terminal_id: ui
                        .terminal_id
                        .clone()
                        .context("missing reconnect terminal")?,
                    writer_resume_token: requests.writer_resume_token,
                };
            } else if let Some(previous) = &ui.reconnect_target {
                target = previous.clone();
            }
        } else {
            target = AttachTarget::Pane(selected, false);
            requests.terminal = ui.terminal_id.clone();
        }
        explicit = false;
        reconnecting = true;
        let retry_target = ssh.then(|| target.clone());
        shared.update_ui(move |ui| {
            ui.connected = false;
            ui.inventory_ready = false;
            ui.inventory_offline = false;
            ui.changing = ssh;
            ui.reconnect_target = retry_target;
            ui.message = "Reconnecting to the existing Corvid terminal…".into();
        });
        // Disable new input before draining requests for the retired writer.
        while requests.actions.try_recv().is_ok() {}
        if let Some(retry) = &mut retry {
            let Some(next) = retry.next_attempt(&error) else {
                return Err(SshRetry::exhausted().into());
            };
            corvid_async::time::sleep_until(next).await;
            ensure!(Instant::now() < retry.deadline(), SshRetry::exhausted());
        } else {
            corvid_async::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

pub(crate) async fn prepare_destination(
    destination: &Destination,
    state_dir: &Path,
    capabilities: Vec<String>,
    explicit: bool,
    expected_host: Option<&str>,
    prompts: &Prompts,
    progress: impl Fn(&str),
) -> Result<(ControlEndpoint, InteractiveConnection)> {
    if let Some(remote) = destination.remote() {
        let configured_host = Config::read_if_present(state_dir)?.and_then(|config| {
            let matches = match (&config.endpoint, &remote) {
                (HostEndpoint::Ssh { alias }, RemoteHost::Ssh(target)) => alias == target.as_str(),
                (HostEndpoint::Wsl { distribution }, RemoteHost::Wsl(target)) => {
                    distribution == target.as_str()
                }
                _ => false,
            };
            matches.then(|| config.host_id.to_string())
        });
        let expected_host = expected_host.or(configured_host.as_deref());
        let (remote, link) = remote_bootstrap::connect(
            remote,
            capabilities,
            explicit,
            expected_host,
            prompts,
            progress,
        )
        .await?;
        Ok((remote.endpoint(), link))
    } else {
        local_bootstrap::connect(capabilities, explicit, expected_host, prompts, progress).await
    }
}

async fn run(
    destination: Destination,
    state_dir: PathBuf,
    mut target: AttachTarget,
    explicit: bool,
    Requests {
        monitor,
        recorder,
        view_store,
        actions,
        sizes,
        activation,
        prompts,
        host,
        terminal,
        writer_resume_token,
        closing,
        parked,
        attached: attach_ready,
        wake_monitor,
    }: &mut Requests,
    shared: &Arc<Shared>,
    agent_deck: &mut agent_deck::Job,
) -> Result<String> {
    // Older daemons can attach without supporting agent conversation restoration.
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
        AttachTarget::Workspace { host_id, .. }
        | AttachTarget::NewWorkspace { host_id, .. }
        | AttachTarget::Takeover { host_id, .. }
        | AttachTarget::OpenPane { host_id, .. }
        | AttachTarget::Reconnect { host_id, .. }
        | AttachTarget::ResumeAgents { host_id, .. } => Some(host_id.clone()),
        AttachTarget::Restore(saved) => Some(saved.host_id.to_string()),
        AttachTarget::Agent { run, .. } => Some(run.host_id.clone()),
        _ => None,
    };
    let progress = |message: &str| {
        let message = message.to_owned();
        shared.update_ui(move |ui| ui.message = message);
    };
    let expected_epoch = match &target {
        AttachTarget::Agent { run, .. } => Some(run.remote_epoch.as_str()),
        AttachTarget::ResumeAgents {
            automatic_epoch, ..
        } => automatic_epoch.as_deref(),
        _ => None,
    };
    let warmed = parked
        .take(
            &destination,
            host.as_deref().or(known_host.as_deref()),
            expected_epoch,
        )
        .await;
    let (endpoint, mut link) = if let Some(connection) = warmed {
        connection
    } else if let AttachTarget::NewWorkspace { endpoint, .. }
    | AttachTarget::Agent { endpoint, .. } = &target
    {
        (
            endpoint.clone(),
            endpoint.interactive(capabilities.clone()).await?,
        )
    } else {
        prepare_destination(
            &destination,
            &state_dir,
            capabilities.clone(),
            explicit,
            host.as_deref().or(known_host.as_deref()),
            prompts,
            progress,
        )
        .await?
    };
    ensure!(
        host.as_deref()
            .or(known_host.as_deref())
            .is_none_or(|host| host == link.session().host_id),
        ProtocolFault::new(
            "HostMismatch",
            "the connection points to a different Corvid host"
        )
    );
    ensure!(
        expected_epoch.is_none_or(|epoch| epoch == link.session().server_epoch),
        ProtocolFault::new(
            "EpochMismatch",
            "The agent belongs to a previous daemon session."
        )
    );
    *host = Some(link.session().host_id.clone());
    let reconnect_attempt = matches!(target, AttachTarget::Reconnect { .. });
    let mut resume_token = None;
    let reconnecting_terminal = match target {
        AttachTarget::Reconnect {
            view,
            epoch,
            terminal_id,
            writer_resume_token,
            ..
        } => {
            *terminal = (epoch == link.session().server_epoch).then_some(terminal_id);
            if terminal.is_some()
                && link
                    .session()
                    .capabilities
                    .iter()
                    .any(|cap| cap == WRITER_RESUME_CAPABILITY)
            {
                resume_token = writer_resume_token;
            }
            target = AttachTarget::Pane(view, false);
            terminal.is_some()
        }
        _ => false,
    };
    // Keep the verified endpoint available to Sessions even when attach is busy.
    let displayed_host = host.clone();
    let displayed_epoch = link.session().server_epoch.clone();
    let displayed_endpoint = endpoint.clone();
    let restore_supported = link
        .session()
        .capabilities
        .iter()
        .any(|cap| cap == corvid_protocol::RESTORE_AGENTS_CAPABILITY);
    let recovery_supported = AttachRecovery::supported(link.session());
    shared.update_ui(move |ui| {
        ui.host_id = displayed_host;
        ui.recovery_supported = recovery_supported;
        ui.epoch = Some(displayed_epoch);
        ui.endpoint = Some(displayed_endpoint);
        ui.restore_supported = restore_supported;
    });
    let restoring = matches!(target, AttachTarget::Restore(_));
    let offer_recovery =
        !reconnecting_terminal && !matches!(target, AttachTarget::ResumeAgents { .. });
    let opening_pane = matches!(target, AttachTarget::OpenPane { .. });
    if let AttachTarget::OpenPane { view, .. } = target {
        target = AttachTarget::Pane(view, true);
    }
    if let AttachTarget::Restore(saved) = &target {
        let snapshot = endpoint
            .view_snapshot(&link.session().host_id, &link.session().server_epoch)
            .await?;
        let inventory = Arc::new(Inventory::read(&snapshot));
        shared.update_ui(move |ui| {
            ui.inventory = Some(inventory);
            ui.inventory_ready = true;
        });
        let view = saved.resolve(&snapshot)?;
        let selected = view.clone();
        shared.update_ui(move |ui| ui.selected = Some(selected));
        target = AttachTarget::Pane(view, false);
    }
    if let AttachTarget::ResumeAgents {
        view,
        pane_only,
        automatic_epoch,
        ..
    } = &target
    {
        let view = view.clone();
        let snapshot = endpoint
            .view_snapshot(&link.session().host_id, &link.session().server_epoch)
            .await?;
        let selected = view.clone();
        let inventory = Arc::new(Inventory::read(&snapshot));
        shared.update_ui(move |ui| {
            ui.inventory = Some(inventory);
            ui.inventory_ready = true;
            ui.selected = Some(selected);
            ui.message = "Restoring saved agent sessions…".into();
        });
        let automatic = automatic_epoch.is_some();
        let mode = if automatic {
            corvid_protocol::wire::AgentRestoreMode::Resume
        } else {
            corvid_protocol::wire::AgentRestoreMode::Retry
        };
        let mut result = link
            .restore_workspace(corvid_protocol::wire::RestoreWorkspace {
                workspace_id: view.workspace_id.clone(),
                pane_id: pane_only.then(|| view.pane_id.clone()),
                mode: mode as i32,
            })
            .await?;
        if automatic {
            let pane = result.panes.first().context("missing recovered pane")?;
            if !matches!(
                pane.status(),
                corvid_protocol::wire::PaneRestoreStatus::Resumed
                    | corvid_protocol::wire::PaneRestoreStatus::Existing
            ) {
                let message = pane
                    .error
                    .as_ref()
                    .map_or("Could not restore the saved agent conversation.", |error| {
                        error.message.as_str()
                    })
                    .to_owned();
                shared.update_ui(move |ui| ui.restore_results = result.panes);
                return Ok(message);
            }
            *terminal = Some(
                pane.terminal_id
                    .clone()
                    .context("missing recovered terminal")?,
            );
        } else if *pane_only {
            result = link
                .restore_workspace(corvid_protocol::wire::RestoreWorkspace {
                    workspace_id: view.workspace_id.clone(),
                    pane_id: None,
                    mode: corvid_protocol::wire::AgentRestoreMode::Inspect as i32,
                })
                .await?;
        }
        let restore_results = result.panes.clone();
        shared.update_ui(move |ui| ui.restore_results = restore_results);
        // A workspace can recover partially. Prefer the requested pane, then a live sibling.
        let restored = result.panes.iter().filter(|pane| {
            matches!(
                pane.status(),
                corvid_protocol::wire::PaneRestoreStatus::Resumed
                    | corvid_protocol::wire::PaneRestoreStatus::Existing
            )
        });
        let selected = restored
            .clone()
            .find(|pane| pane.location.as_ref() == Some(&view))
            .or_else(|| (!*pane_only).then(|| restored.into_iter().next()).flatten())
            .and_then(|pane| pane.location.clone())
            .unwrap_or(view);
        target = AttachTarget::Pane(selected, false);
    }
    let grid = sizes.borrow().2;
    let resume = terminal.is_none()
        && !restoring
        && !reconnect_attempt
        && shared.snapshot().ui.restore_results.is_empty();
    let attached = match &target {
        AttachTarget::Agent { run, .. } => attach_agent(&endpoint, &mut link, grid, run).await?,
        AttachTarget::Restore(_)
        | AttachTarget::OpenPane { .. }
        | AttachTarget::Reconnect { .. } => {
            unreachable!("pane resolved before attach")
        }
        AttachTarget::ResumeAgents { .. } => unreachable!("agent restore resolved before attach"),
        AttachTarget::NewWorkspace {
            label, directory, ..
        } => {
            attach(
                &mut link,
                grid,
                false,
                Attach {
                    create_workspace: Some(label.clone()),
                    workspace_directory: directory.clone(),
                    launch: Some(LaunchCommand::default()),
                    ..Default::default()
                },
            )
            .await?
        }
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
            if new_tab.is_none() {
                AttachRecovery::request(link.session(), &mut request);
            }
            let epoch = link.session().server_epoch.clone();
            let mut preferred = None;
            if new_tab.is_none()
                && let Some(saved) =
                    view_store.for_workspace(&destination.saved_target(), host_id, id)
                && let Ok(snapshot) = endpoint.view_snapshot(host_id, &epoch).await
            {
                preferred = saved.preferred_live_pane(&snapshot);
            }
            loop {
                let mut attempt = request.clone();
                if let Some(view) = &preferred {
                    attempt.tab_id = Some(view.tab_id.clone());
                    attempt.pane_id = Some(view.pane_id.clone());
                }
                match attach(&mut link, grid, resume, attempt).await {
                    Ok(attached) => {
                        ensure!(
                            preferred
                                .as_ref()
                                .is_none_or(|view| attached.selection.as_ref() == Some(view)),
                            "daemon attached to a different pane than requested"
                        );
                        break attached;
                    }
                    Err(error) if preferred.is_some() && workspace_preference_missed(&error) => {
                        preferred = None;
                    }
                    Err(error)
                        if new_tab.is_none()
                            && request.launch.is_none()
                            && error
                                .downcast_ref::<ProtocolFault>()
                                .is_some_and(|fault| fault.code == "NoTerminal") =>
                    {
                        request.launch = Some(LaunchCommand::default());
                    }
                    Err(error) => return Err(error),
                }
                drop(link);
                link = endpoint.interactive(capabilities.clone()).await?;
                ensure!(
                    link.session().host_id == *host_id && link.session().server_epoch == epoch,
                    "The Corvid daemon changed while opening the workspace"
                );
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
            let mut request = Attach {
                workspace_id: Some(view.workspace_id.clone()),
                tab_id: Some(view.tab_id.clone()),
                pane_id: Some(view.pane_id.clone()),
                launch: open.then(LaunchCommand::default),
                writer_resume_token: resume_token.map(|token| token.as_bytes().to_vec()),
                expected_terminal_id: resume_token.and(terminal.clone()),
                ..Default::default()
            };
            if (!open && offer_recovery) || opening_pane {
                AttachRecovery::request(link.session(), &mut request);
            }
            attach(&mut link, grid, resume, request).await?
        }
        AttachTarget::Default | AttachTarget::FreshDefault => {
            let mut request = Attach {
                terminal_capabilities: vec![DEFAULT_SHELL_CAPABILITY.into()],
                launch: Some(LaunchCommand::default()),
                ..Default::default()
            };
            AttachRecovery::request(link.session(), &mut request);
            attach(&mut link, grid, resume, request).await?
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
                AttachTarget::Restore(_) | AttachTarget::OpenPane { .. } => false,
                AttachTarget::ResumeAgents { .. } | AttachTarget::Reconnect { .. } => false,
                AttachTarget::Default
                | AttachTarget::FreshDefault
                | AttachTarget::NewWorkspace { .. }
                | AttachTarget::Agent { .. } => true,
                AttachTarget::Pane(view, _) | AttachTarget::Takeover { view, .. } =>
                    *view == selected,
                AttachTarget::Workspace { id, .. } => *id == selected.workspace_id,
            },
        "invalid initial daemon route"
    );
    *writer_resume_token = attached
        .writer_resume_token
        .as_deref()
        .map(Uuid::from_slice)
        .transpose()
        .context("invalid daemon writer resume token")?;

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
    let split_target_supported = link
        .session()
        .capabilities
        .iter()
        .any(|capability| capability == corvid_protocol::WORKSPACE_UI_CAPABILITY);
    let naming_supported = link
        .session()
        .capabilities
        .iter()
        .any(|name| name == corvid_protocol::VIEW_TITLES_CAPABILITY);
    let tab_preview_supported = link
        .session()
        .capabilities
        .iter()
        .any(|name| name == corvid_protocol::TAB_PREVIEW_CAPABILITY);
    shared.update_ui(move |ui| {
        ui.tab_preview_supported = tab_preview_supported;
        ui.close_supported = close_supported;
        ui.naming_supported = naming_supported;
        ui.split_target_supported = split_target_supported;
        ui.session_clients_supported = session_clients_supported;
    });
    let monitor_host = Uuid::parse_str(&host)?;
    let wake_monitor = std::mem::take(wake_monitor)
        || (reconnect_attempt && matches!(endpoint, ControlEndpoint::Ssh(_)));
    let mut monitor_reconnect = shared.ssh_control_reconnect.subscribe();
    let (focus_scope, mut focus_controller) = mpsc::channel(1);
    let mut monitor_tasks = corvid_async::task::JoinSet::new();
    monitor_tasks.spawn(refresh_agent_monitor(
        monitor.clone(),
        endpoint.clone(),
        monitor_host,
        wake_monitor,
        focus_scope.clone(),
    ));
    let (deck_requests, mut deck_focus) = mpsc::channel(1);
    let focus_epoch = Uuid::parse_str(&epoch)?;
    let focus_requests = deck_requests.clone();
    let mut focus_service = corvid_async::task::JoinSet::new();
    focus_service.spawn(async move {
        if let Some(controller) = focus_controller.recv().await {
            drop(focus_controller);
            // Another window may already own the host's private listener.
            // If it closes, a remaining attached window can take over.
            while !focus_requests.is_closed() {
                let _ = corvid_presentation::desktop_focus::serve(
                    monitor_host,
                    controller,
                    focus_epoch,
                    focus_requests.clone(),
                )
                .await;
                corvid_async::time::sleep(Duration::from_secs(1)).await;
            }
        }
    });
    let mut client = Client {
        recorder: recorder.as_ref(),
        record_navigation: !matches!(target, AttachTarget::Agent { .. }),
        saved_target: destination.saved_target(),
        link,
        endpoint: endpoint.clone(),
        agent_deck,
        deck_requests,
        deck_result: None,
        shared,
        selected,
        route,
        grid,
        parser: Decoder::new(grid.rows, grid.cols),
        applied: None,
        sequence: 0,
        pending: None,
        view_change: None,
        activation: activation.clone(),
        preview: None,
        history: None,
        queued_history: None,
        next_history: Instant::now(),
        preview_index: 0,
        next_preview: Instant::now(),
    };
    client.publish_view();
    if let Some(attached) = attach_ready.take() {
        let _ = attached.send(());
    }
    let mut inventory = InventoryWatch::start(endpoint.clone(), host, epoch, Arc::clone(shared))?;
    let mut tick = corvid_async::time::interval(Duration::from_millis(100));
    let keep_connection = loop {
        corvid_async::select! {
            close = &mut *closing => break close.unwrap_or(false),
            Ok(()) = monitor_reconnect.changed() if monitor_tasks.is_empty() => {
                monitor_tasks.spawn(refresh_agent_monitor(
                    monitor.clone(),
                    endpoint.clone(),
                    monitor_host,
                    true,
                    focus_scope.clone(),
                ));
            }
            Some(result) = monitor_tasks.join_next() if !monitor_tasks.is_empty() => {
                if let Err(error) = result.unwrap_or_else(|error| Err(error.into())) {
                    shared.update_ui(move |ui| ui.message = format!("Agent monitor unavailable: {error:#}"));
                }
            }
            frame = client.link.receive() => {
                if let Some(message) = client.receive(frame?).await? {
                    return Ok(message);
                }
            }
            action = actions.recv() => match action {
                Some(action) => client.action(action).await?,
                None => break false,
            },
            Some(request) = deck_focus.recv() => client.focus_from_deck(request).await?,
            result = client.agent_deck.tasks.join_next() if !client.agent_deck.tasks.is_empty() => {
                match result {
                    Some(Ok(Err(error))) => shared.update_ui(move |ui| ui.message = format!("{error:#}")),
                    Some(Err(_)) => shared.update_ui(|ui| ui.message = "Corvid Agent Deck stopped unexpectedly.".into()),
                    _ => {}
                }
            }
            _ = tick.tick() => {
                let size = *sizes.borrow();
                client.tick(size).await?;
            }
            _ = &mut inventory.finished => bail!("workspace synchronization stopped"),
        }
        if client.pending.is_none() {
            client.view_change = None;
        }
    };
    let keep_connection = keep_connection
        && client
            .link
            .session()
            .capabilities
            .iter()
            .any(|cap| cap == corvid_protocol::PARK_TERMINAL_CAPABILITY);
    match timeout(DETACH_TIMEOUT, client.detach(keep_connection)).await {
        Ok(Ok(true)) => parked.insert(destination, endpoint, client.link).await,
        Ok(Ok(false)) => {}
        Ok(Err(error)) => eprintln!("Desktop detach was not acknowledged: {error:#}"),
        Err(_) => eprintln!("Timed out waiting for Desktop detach"),
    }
    Ok("Detached from Corvid.".into())
}

async fn refresh_agent_monitor(
    settings: agent_deck::Settings,
    endpoint: ControlEndpoint,
    host: Uuid,
    reconnect: bool,
    focus_scope: mpsc::Sender<Uuid>,
) -> Result<()> {
    let scope = settings.scope(&endpoint, host)?;
    scope.configure(&endpoint)?;
    let _ = focus_scope.try_send(scope.config.controller_id);
    scope
        .ensure_running(&corvid_tui::monitor_executable()?)
        .await?;
    if reconnect {
        scope.reconnect().await?;
    }
    Ok(())
}

async fn attach_agent(
    endpoint: &ControlEndpoint,
    link: &mut InteractiveConnection,
    grid: Grid,
    run: &corvid_protocol::wire::PresentedRunKey,
) -> Result<Attached> {
    if link
        .session()
        .capabilities
        .iter()
        .any(|cap| cap == corvid_protocol::ATTACH_AGENT_CAPABILITY)
    {
        return attach(
            link,
            grid,
            false,
            Attach {
                agent_run_id: Some(run.agent_run_id.clone()),
                ..Default::default()
            },
        )
        .await;
    }
    // Verify the exact pane and run on both sides of attach on older daemons.
    let (view, terminal) = agent_location(endpoint, run).await?;
    let attached = attach(
        link,
        grid,
        false,
        Attach {
            workspace_id: Some(view.workspace_id.clone()),
            tab_id: Some(view.tab_id.clone()),
            pane_id: Some(view.pane_id.clone()),
            ..Default::default()
        },
    )
    .await?;
    ensure!(
        attached.selection.as_ref() == Some(&view)
            && attached
                .route_token
                .as_ref()
                .is_some_and(|route| route.terminal_id == terminal)
            && agent_location(endpoint, run).await? == (view, terminal),
        ProtocolFault::new("TargetGone", "The agent pane is no longer available.")
    );
    Ok(attached)
}

async fn agent_location(
    endpoint: &ControlEndpoint,
    run: &corvid_protocol::wire::PresentedRunKey,
) -> Result<(ViewSelection, String)> {
    let snapshot = endpoint
        .view_snapshot(&run.host_id, &run.remote_epoch)
        .await?;
    let inventory = Inventory::read(&snapshot);
    inventory
        .agents
        .into_iter()
        .find(|agent| agent.agent_run_id == run.agent_run_id && agent.terminal_available)
        .and_then(|agent| {
            let view = agent.location?;
            let terminal = inventory
                .terminals
                .iter()
                .find(|terminal| terminal.location.as_ref() == Some(&view))?;
            Some((view, terminal.terminal_id.clone()))
        })
        .ok_or_else(|| {
            ProtocolFault::new("TargetGone", "The agent pane is no longer available.").into()
        })
}

async fn attach(
    link: &mut InteractiveConnection,
    grid: Grid,
    resume: bool,
    mut request: Attach,
) -> Result<Attached> {
    if request.workspace_directory.is_some() {
        ensure!(
            link.session()
                .capabilities
                .iter()
                .any(|name| name == corvid_protocol::WORKSPACE_DIRECTORY_CAPABILITY),
            ProtocolFault::new(
                "UnsupportedCapability",
                "Update Corvid on this host to choose a workspace folder."
            )
        );
    }
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
    if link
        .session()
        .capabilities
        .iter()
        .any(|cap| cap == CLIPBOARD_WRITE_CAPABILITY)
    {
        request
            .terminal_capabilities
            .push(CLIPBOARD_WRITE_CAPABILITY.into());
    }
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
    if link
        .session()
        .capabilities
        .iter()
        .any(|cap| cap == WRITER_RESUME_CAPABILITY)
    {
        request
            .terminal_capabilities
            .push(WRITER_RESUME_CAPABILITY.into());
    }
    if request.requested_role == AttachRole::Unspecified as i32 {
        request.requested_role = AttachRole::Writer as i32;
    }
    let recovery_requested = request
        .terminal_capabilities
        .iter()
        .any(|cap| cap == corvid_protocol::ATTACH_RECOVERY_CAPABILITY);
    let id = link.send(Body::Attach(request.clone())).await?;
    let reply = timeout(COMMAND_TIMEOUT, link.receive()).await??;
    let (reply_id, reply) = envelope(link, reply)?;
    ensure!(
        reply_id.as_deref() == Some(&id),
        "unexpected attach response ID"
    );
    match reply {
        Body::Attached(attached) => Ok(attached),
        Body::WorkspaceRestored(reply) if recovery_requested => {
            Err(AttachRecovery::from_reply(&request, reply)?.into())
        }
        Body::Error(error) => Err(ProtocolFault::from_wire(&error).into()),
        _ => bail!("unexpected attach response"),
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
    async fn detach(&mut self, keep_connection: bool) -> Result<bool> {
        if !self
            .link
            .session()
            .capabilities
            .iter()
            .any(|cap| cap == GRACEFUL_DETACH_CAPABILITY)
        {
            return Ok(false);
        }
        let id = self
            .link
            .send(Body::GracefulDetach(
                corvid_protocol::wire::GracefulDetach { keep_connection },
            ))
            .await?;
        loop {
            let Frame::Structured(payload) = self.link.receive().await? else {
                continue;
            };
            let reply = decode_envelope(&payload)?;
            self.link.session().validate_scope(&reply)?;
            if reply.request_id.as_deref() == Some(id.as_str()) {
                return match reply.body {
                    Some(Body::Pong(_)) if keep_connection => Ok(true),
                    Some(Body::Detach(_)) => Ok(false),
                    Some(Body::Error(fault)) => Err(ProtocolFault::from_wire(&fault).into()),
                    _ => bail!("unexpected daemon detach response"),
                };
            }
        }
    }
    fn record_view(&self) {
        if self.record_navigation
            && let Some(recorder) = self.recorder
        {
            recorder.record(
                self.saved_target.clone(),
                &self.link.session().host_id,
                &self.selected,
            );
        }
    }

    fn publish_view(&self) {
        self.shared.clipboard.send_replace(None);
        self.record_view();
        let selected = self.selected.clone();
        let terminal_id = self.route.terminal_id.clone();
        let changing = self.pending.is_some();
        let revision = self.route.view_revision;
        self.shared.edit(move |state| {
            let ui = Arc::make_mut(&mut state.ui);
            if !ui.connected
                || ui
                    .selected
                    .as_ref()
                    .is_some_and(|previous| previous.tab_id != selected.tab_id)
            {
                state.previews = Arc::new(HashMap::new());
            }
            ui.selected = Some(selected);
            ui.terminal_id = Some(terminal_id);
            ui.connected = true;
            ui.reconnect_target = None;
            ui.changing = changing;
            ui.message = "Attached to Corvid daemon".into();
            state.revision = revision;
            state.view_epoch = state.view_epoch.wrapping_add(1);
            state.screen = None;
            Changed::View
        });
    }

    async fn tick(&mut self, (view_epoch, revision, grid): (u64, u64, Grid)) -> Result<()> {
        let now = Instant::now();
        if let Some((_, preview)) = &self.preview {
            ensure!(now < preview.deadline, "daemon pane preview timed out");
        }
        if let Some(history) = &self.history {
            ensure!(now < history.deadline, "daemon terminal history timed out");
        }
        if self.history.is_none()
            && now >= self.next_history
            && let Some(read) = self.queued_history.take()
        {
            self.read_history(read).await?;
        }
        if let Some(pending) = &self.pending {
            ensure!(now < pending.deadline, "daemon view change timed out");
        } else if self.matches_view(view_epoch, revision) && self.grid != grid {
            self.resize(grid).await?;
        } else if self.preview.is_none() && now >= self.next_preview && self.applied.is_some() {
            self.next_preview = now + PREVIEW_INTERVAL;
            self.read_preview().await?;
        }
        Ok(())
    }

    fn preview_panes(&self, ui: &Ui) -> Vec<String> {
        let preview_tab = self.shared.preview_tab.borrow();
        let tab = preview_tab
            .as_deref()
            .filter(|_| ui.tab_preview_supported)
            .unwrap_or(&self.selected.tab_id);
        ui.inventory.as_ref().map_or_else(Vec::new, |inventory| {
            inventory.live_preview_panes(&self.selected, tab)
        })
    }

    async fn read_preview(&mut self) -> Result<()> {
        let snapshot = self.shared.snapshot();
        let panes = self.preview_panes(&snapshot.ui);
        if snapshot.previews.keys().any(|pane| !panes.contains(pane)) {
            let keep = panes.clone();
            self.shared.edit(move |state| {
                Arc::make_mut(&mut state.previews).retain(|pane, _| keep.contains(pane));
                Changed::Ui
            });
        }
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

    fn matches_view(&self, view_epoch: u64, revision: u64) -> bool {
        revision == self.route.view_revision && view_epoch == self.shared.snapshot().view_epoch
    }

    async fn action(&mut self, action: Action) -> Result<()> {
        let action = match action {
            Action::Change {
                action,
                view_epoch,
                reservation,
            } => {
                if self.pending.is_some() || view_epoch != self.shared.snapshot().view_epoch {
                    return Ok(());
                }
                self.view_change = Some(reservation);
                *action
            }
            action => action,
        };
        match action {
            Action::Change { .. } => unreachable!("view change already unwrapped"),
            Action::FocusAgent(request) => self.focus_from_deck(request).await?,
            Action::CommitNavigation => {
                self.record_navigation = true;
                self.record_view();
            }

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
            Action::ReadHistory(read) => {
                if self.history.is_some() || Instant::now() < self.next_history {
                    self.queued_history = Some(read);
                } else {
                    self.queued_history = None;
                    self.read_history(read).await?;
                }
            }
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
                view_epoch,
                revision,
                bytes,
                paste,
            } if self.pending.is_none()
                && self.applied.is_some()
                && self.matches_view(view_epoch, revision) =>
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

    async fn read_history(&mut self, read: HistoryRead) -> Result<()> {
        let HistoryRead {
            view_epoch,
            revision,
            shell_only,
            reply,
        } = read;
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
        } else if self.pending.is_some() || !self.matches_view(view_epoch, revision) {
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
                shell_only,
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
        if request.result.is_closed() {
            return Ok(());
        }
        let run = request.run;
        let session = self.link.session();
        if run.host_id != session.host_id || run.remote_epoch != session.server_epoch {
            let _ = request.result.send(Err(
                "The agent belongs to a different daemon session.".into()
            ));
            return Ok(());
        }
        let state = self.shared.snapshot();
        let reservation = if state.ui.changing || self.pending.is_some() {
            None
        } else {
            self.shared.reserve_view_change()
        };
        let Some(reservation) = reservation else {
            let _ = request
                .result
                .send(Err("Desktop is changing views. Try again.".into()));
            return Ok(());
        };
        let selection = state
            .ui
            .inventory
            .as_ref()
            .filter(|_| state.ui.inventory_ready)
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
        self.view_change = Some(reservation);
        self.shared.ui_changed.send_replace(());
        self.deck_result = Some(request.result);
        self.begin_select(selection, false, Some(run.agent_run_id))
            .await
    }

    fn finish_deck_focus(&mut self, result: Result<(), FocusFailure>) {
        let Some(reply) = self.deck_result.take() else {
            return;
        };
        if let Err(error) = result {
            let _ = reply.send(Err(error));
            return;
        }
        let activation = DeckActivation {
            view_epoch: self.shared.snapshot().view_epoch,
            revision: self.route.view_revision,
            reply,
        };
        if let Err(error) = self.activation.try_send(activation) {
            let _ = error
                .into_inner()
                .reply
                .send(Err("Desktop could not receive focus.".into()));
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
        let buffer = self.shared.snapshot();
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
        let target = if matches!(target, Target::TabLabel(_))
            && self.shared.snapshot().ui.naming_supported
        {
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
        let Some(reservation) = self.shared.reserve_view_change() else {
            return Ok(());
        };
        self.view_change = Some(reservation);
        self.shared.ui_changed.send_replace(());
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
            // A quick down/up gesture or explicit shortcut can follow an in-flight read.
            self.next_history = Instant::now() + corvid_protocol::TERMINAL_HISTORY_INTERVAL;
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
            Body::ClipboardWrite(clipboard) => {
                let snapshot = self.shared.snapshot();
                if clipboard.route_token.as_ref() == Some(&self.route)
                    && self.pending.is_none()
                    && snapshot.ui.can_write()
                    && !snapshot.ui.changing
                    && corvid_protocol::valid_clipboard_text(&clipboard.text)
                {
                    self.shared.clipboard.send_replace(Some(ClipboardWrite {
                        view_epoch: snapshot.view_epoch,
                        text: clipboard.text,
                    }));
                }
            }
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
                    let grid = Grid::read(view.viewport.context("view update has no viewport")?)?;
                    self.grid = grid;
                    self.parser = Decoder::new(grid.rows, grid.cols);
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
                let fault = ProtocolFault::from_wire(&error);
                let message = fault.to_string();
                if matched {
                    self.finish_deck_focus(Err(fault.into()));
                }
                self.shared.update_ui(move |ui| {
                    if matched {
                        ui.changing = false;
                    }
                    ui.message = message;
                });
            }
            Body::TerminalExited(exit) if exit.terminal_id == self.route.terminal_id => {
                let terminal_id = exit.terminal_id;
                self.shared.update_ui(move |ui| {
                    if let Some(inventory) = &mut ui.inventory {
                        Arc::make_mut(inventory)
                            .terminals
                            .retain(|terminal| terminal.terminal_id != terminal_id);
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
                self.shared.update_ui(move |ui| {
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
            if !self
                .preview_panes(&self.shared.snapshot().ui)
                .contains(&expected)
            {
                return Ok(());
            }
            if let Ok(grid) = Grid::read(preview.viewport.context("pane preview has no size")?) {
                let mut parser = Decoder::new(grid.rows, grid.cols);
                parser.process(&frame.ansi);
                let screen = Arc::new(parser.screen().clone());
                self.shared.edit(move |state| {
                    Arc::make_mut(&mut state.previews).insert(expected, screen);
                    Changed::Ui
                });
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
        self.shared.apply_ansi(
            &mut self.parser,
            &frame.ansi,
            header.wheel_encoding(),
            header.alternate_screen,
        );
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

fn wheel_report(
    encoding: WheelEncoding,
    (row, col): (u16, u16),
    rows: isize,
    modifiers: u8,
) -> Option<Vec<u8>> {
    let button = (if rows < 0 { 64 } else { 65 }) | modifiers;
    let x = u32::from(col) + 1;
    let y = u32::from(row) + 1;
    let report = match encoding {
        WheelEncoding::Disabled => return None,
        WheelEncoding::Sgr => format!("\x1b[<{button};{x};{y}M").into_bytes(),
        WheelEncoding::Legacy => vec![
            0x1b,
            b'[',
            b'M',
            button + 32,
            (x.min(223) + 32) as u8,
            (y.min(223) + 32) as u8,
        ],
        WheelEncoding::Utf8 => {
            let x = char::from_u32(x.min(2015) + 32).unwrap();
            let y = char::from_u32(y.min(2015) + 32).unwrap();
            format!("\x1b[M{}{x}{y}", char::from(button + 32)).into_bytes()
        }
    };
    Some(report.repeat(rows.unsigned_abs().min(32)))
}

fn validate_history(history: &TerminalHistory) -> Result<()> {
    let mut size = history
        .lines
        .iter()
        .map(|line| line.len() + 8)
        .sum::<usize>();
    ensure!(
        (1..=1024).contains(&history.columns)
            && history.lines.len() <= TERMINAL_HISTORY_MAX_ROWS
            && size <= TERMINAL_HISTORY_MAX_BYTES
            && history
                .lines
                .iter()
                .all(|line| !line.chars().any(char::is_control)),
        "invalid terminal history"
    );
    let mut previous = (0, 0);
    for style in &history.styles {
        let position = (style.row, style.offset);
        size += style.sgr.len() + 20;
        ensure!(
            size <= TERMINAL_HISTORY_MAX_BYTES
                && position >= previous
                && history
                    .lines
                    .get(style.row as usize)
                    .is_some_and(|line| line.is_char_boundary(style.offset as usize))
                && style.sgr.len() <= 96
                && style
                    .sgr
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b';'),
            "invalid terminal history style"
        );
        previous = position;
    }
    Ok(())
}
