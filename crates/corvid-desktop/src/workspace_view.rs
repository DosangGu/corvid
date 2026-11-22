use std::cell::RefCell;
mod chrome;
mod connections;
mod deck_focus;
mod host_watch;
mod management;
mod naming;
mod navigation;
mod switcher;
mod tab_bar;
mod title;
mod update;
#[cfg(windows)]
mod window_controls;
mod workspace_creation;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(windows)]
use corvid_client::WslDistribution;
use corvid_client::{ControlEndpoint, SshAlias};
use corvid_protocol::wire::create_view::Target;
use corvid_protocol::wire::layout_node_state::Body as Node;
use corvid_protocol::wire::{CloseView, SplitAxis, TabState, ViewSelection, WorkspaceState};
use gpui::{
    App, AppContext, AsyncWindowContext, Context, Div, Entity, FocusHandle, IntoElement,
    KeyDownEvent, PromptButton, PromptLevel, SharedString, Stateful, Task, Window, WindowHandle,
    div, prelude::*, rgb,
};

use crate::agent_deck;
use crate::appearance::{self, Appearance, ui_px};
use crate::desktop_windows::{DesktopWindows, WindowLaunch};
use crate::ssh_hosts;
use crate::terminal_session::{
    AttachTarget, Connection, Destination, Inventory, Job, ParkedConnections, Session, Ui,
};
use crate::terminal_view::{TerminalView, preview};
use corvid_async::{oneshot, watch};
use corvid_client::view_state::{SavedView, ViewStore};

const BACKGROUND: u32 = 0x101318;
const PANEL: u32 = 0x191c23;
const BORDER: u32 = 0x30343f;
const TEXT: u32 = 0xe3e8ee;
const MUTED: u32 = 0xa4adbc;
const ACTIVE: u32 = 0x83dec0;
const ACTIVE_FILL: u32 = 0x20332f;
const NAVIGATION_HIGHLIGHT: u32 = 0xf3c771;
const MAX_BACKGROUND_HOSTS: usize = 8;

async fn watch_on_frames<T: Clone + 'static>(
    mut changes: watch::Receiver<T>,
    cx: &mut AsyncWindowContext,
    update: impl Fn(T, &mut Window, &mut App) -> bool + Clone + 'static,
) {
    while changes.changed().await.is_ok() {
        let (complete, frame) = oneshot::channel();
        let update = update.clone();
        cx.on_next_frame(move |window, cx| {
            if complete.is_closed() {
                return;
            }
            let latest = changes.borrow_and_update().clone();
            if update(latest, window, cx) {
                let _ = complete.send(changes);
            }
        });
        let Ok(receiver) = frame.await else {
            break;
        };
        changes = receiver;
    }
}

#[cfg(all(test, windows))]
mod frame_tests;

#[derive(Clone, Default)]
pub struct Lifetime(Rc<RefCell<WindowConnections>>);

#[derive(Default)]
struct WindowConnections {
    active: Option<Job>,
    preparing: Option<Job>,
    shutdown: Option<watch::Receiver<()>>,
    retiring: Vec<Job>,
    parked: ParkedConnections,
}

impl Lifetime {
    fn parked(&self) -> ParkedConnections {
        self.0.borrow().parked.clone()
    }

    fn replace(&self, job: Job, park_previous: bool) {
        let mut connections = self.0.borrow_mut();
        connections.retiring.retain(|old| !old.finished());
        if let Some(mut old) = connections.active.replace(job) {
            old.stop(park_previous);
            connections.retiring.push(old);
        }
    }

    fn retirement_done(&self) -> bool {
        let connections = self.0.borrow();
        connections.shutdown.is_none() && connections.retiring.iter().all(Job::finished)
    }

    fn available(&self) -> bool {
        let connections = self.0.borrow();
        self.retirement_done()
            && connections.preparing.is_none()
            && connections.active.as_ref().is_none_or(Job::finished)
    }

    pub(crate) fn stop(&self) {
        self.retire(false);
        if let Some(job) = self.0.borrow_mut().preparing.as_mut() {
            job.stop(false);
        }
    }

    fn retire(&self, park: bool) {
        if let Some(job) = self.0.borrow_mut().active.as_mut() {
            job.stop(park);
        }
    }

    fn stage(&self, job: Job) {
        self.0.borrow_mut().preparing = Some(job);
    }

    fn take_staged(&self) -> Option<Job> {
        self.0.borrow_mut().preparing.take()
    }

    fn discard_staged(&self) {
        if let Some(job) = self.take_staged() {
            self.discard_job(job);
        }
    }

    fn discard_job(&self, mut job: Job) {
        job.stop(false);
        self.0.borrow_mut().retiring.push(job);
    }

    pub fn start_shutdown(&self, after: impl FnOnce() + Send + 'static) -> watch::Receiver<()> {
        self.stop();
        let mut connections = self.0.borrow_mut();
        if let Some(shutdown) = &connections.shutdown {
            return shutdown.clone();
        }

        let active = connections.active.take();
        let preparing = connections.preparing.take();
        let retiring = std::mem::take(&mut connections.retiring);
        let parked = std::mem::take(&mut connections.parked);
        let (finished, shutdown) = watch::channel(());
        connections.shutdown = Some(shutdown.clone());
        std::thread::spawn(move || {
            for job in active.into_iter().chain(preparing).chain(retiring) {
                job.join();
            }
            parked.clear();
            after();
            drop(finished);
        });
        shutdown
    }

    fn resume(&self) {
        self.0.borrow_mut().shutdown = None;
    }
}

#[cfg(test)]
#[test]
fn shutdown_tracks_preparing_sessions_and_retains_ownership_until_cleanup() {
    let lifetime = Lifetime::default();
    let (job, stopping, release) = Job::delayed_shutdown();
    lifetime.stage(job);
    let (removed, cleaned) = std::sync::mpsc::channel();

    let shutdown = lifetime.start_shutdown(move || removed.send(()).unwrap());
    let duplicate = lifetime.start_shutdown(|| panic!("shutdown cleanup runs once"));
    assert!(!stopping.recv_timeout(Duration::from_secs(5)).unwrap());
    assert!(!lifetime.available());
    assert!(cleaned.try_recv().is_err());
    assert!(!shutdown.has_changed().unwrap());
    assert!(!duplicate.has_changed().unwrap());

    release.send(()).unwrap();
    cleaned.recv_timeout(Duration::from_secs(5)).unwrap();
    corvid_async::task::block_on(crate::desktop_windows::wait_for_shutdown(vec![
        shutdown, duplicate,
    ]))
    .unwrap();
    lifetime.resume();
    assert!(lifetime.available());
}

pub struct WorkspaceView {
    windows: DesktopWindows,
    store: ViewStore,
    restore_picker_shown: bool,
    naming: Option<naming::NameDraft>,
    creation: Option<workspace_creation::Draft>,
    recent_folders: HashMap<String, Vec<String>>,
    lifetime: Lifetime,
    alias: String,
    setup_focus: FocusHandle,
    management_focus: FocusHandle,
    alias_focus: FocusHandle,
    hosts: std::io::Result<Vec<SshAlias>>,
    host_scan: Option<Task<()>>,
    #[cfg(windows)]
    distributions: anyhow::Result<Vec<WslDistribution>>,
    #[cfg(windows)]
    wsl_scan: Option<Task<()>>,
    host_switch: Option<Task<()>>,
    session: Option<Session>,
    destination: Option<Destination>,
    known_sessions: HashMap<Destination, KnownSessions>,
    host_lookups: HashMap<Destination, Task<()>>,
    lookup_errors: HashMap<Destination, &'static str>,
    terminal: Option<Entity<TerminalView>>,
    selected: Option<ViewSelection>,
    pending_pane_click: Option<(ViewSelection, (u16, u16), Instant)>,
    tab_bar: tab_bar::TabBar,
    preferred_tabs: HashMap<String, String>,
    preferred_panes: HashMap<String, String>,
    updates: Option<Task<()>>,
    events: Option<Task<()>>,
    error: Option<String>,
    agent_deck: agent_deck::Settings,
    switcher_open: bool,
    switcher: switcher::Picker,
    terminal_window: WindowHandle<Self>,
    management_window: Option<WindowHandle<management::ManagementView>>,
    management_page: management::Page,
    zoom_pane: bool,
    navigation: Option<navigation::Overlay>,
    leader: bool,
    menu_open: bool,
    menu_create: bool,
    menu_index: usize,
    release_update: Option<Task<()>>,
    update: update::State,
}

struct KnownSessions {
    host_id: String,
    endpoint: ControlEndpoint,
    inventory: Option<Arc<Inventory>>,
    live: bool,
    watch: Option<(Instant, Task<()>)>,
}

impl WorkspaceView {
    pub fn new(
        launch: WindowLaunch,
        windows: DesktopWindows,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let WindowLaunch {
            lifetime,
            store,
            destination,
            picker,
        } = launch;
        let destination = destination.or_else(|| {
            store
                .last()
                .and_then(|saved| Destination::from_saved(saved.target).ok())
        });
        let agent_deck = windows.settings();
        let setup_focus = cx.focus_handle();
        window.focus(&setup_focus, cx);
        let mut view = Self {
            windows,
            store,
            restore_picker_shown: false,
            naming: None,
            creation: None,
            recent_folders: HashMap::new(),
            lifetime,
            alias: match &destination {
                Some(Destination::Ssh(alias)) => alias.as_str().to_owned(),
                _ => String::new(),
            },
            setup_focus,
            management_focus: cx.focus_handle(),
            alias_focus: cx.focus_handle(),
            hosts: Ok(Vec::new()),
            host_scan: None,
            #[cfg(windows)]
            distributions: Ok(Vec::new()),
            #[cfg(windows)]
            wsl_scan: None,
            host_switch: None,
            session: None,
            destination: None,
            known_sessions: HashMap::new(),
            host_lookups: HashMap::new(),
            lookup_errors: HashMap::new(),
            terminal: None,
            selected: None,
            pending_pane_click: None,
            tab_bar: tab_bar::TabBar::default(),
            preferred_tabs: HashMap::new(),
            preferred_panes: HashMap::new(),
            updates: None,
            events: None,
            error: None,
            agent_deck,
            switcher_open: false,
            switcher: switcher::Picker::default(),
            terminal_window: window.window_handle().downcast().expect("workspace window"),
            management_window: None,
            management_page: management::Page::Connections,
            zoom_pane: false,
            navigation: None,
            leader: false,
            menu_open: false,
            menu_create: false,
            menu_index: 0,
            release_update: None,
            update: update::State::default(),
        };
        cx.on_release(|view, _| view.lifetime.stop()).detach();
        if picker {
            view.show_switcher(window, cx);
        } else {
            view.switch_to(
                destination.unwrap_or(Destination::Local),
                AttachTarget::Default,
                window,
                cx,
            );
        }
        view
    }

    pub(crate) fn resume(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.host_switch = None;
        self.lifetime.resume();
        if let Some(destination) = self.destination.clone() {
            self.connect(destination, AttachTarget::Default, false, window, cx);
        }
    }

    fn new_window(&mut self, cx: &mut Context<Self>) {
        let windows = self.windows.clone();
        let owner = cx.entity().downgrade();
        cx.defer(move |cx| {
            windows.new_window(cx, move |error, cx| {
                let _ = owner.update(cx, |view, cx| {
                    view.error = Some(format!("{error:#}"));
                    cx.notify();
                });
            });
        });
    }

    fn refresh_hosts(&mut self, cx: &mut Context<Self>) {
        if self.host_scan.is_some() {
            return;
        }
        let scan = cx.background_executor().spawn(async { ssh_hosts::load() });
        self.host_scan = Some(cx.spawn(async move |view, cx| {
            let hosts = scan.await;
            let _ = view.update(cx, |view, cx| {
                view.hosts = hosts;
                view.host_scan = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    #[cfg(windows)]
    fn refresh_wsl(&mut self, cx: &mut Context<Self>) {
        if self.wsl_scan.is_some() {
            return;
        }
        let scan = cx
            .background_executor()
            .spawn(async { compio::runtime::Runtime::new()?.block_on(WslDistribution::list()) });
        self.wsl_scan = Some(cx.spawn(async move |view, cx| {
            let distributions = scan.await;
            let _ = view.update(cx, |view, cx| {
                view.distributions = distributions;
                view.wsl_scan = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn connecting(&self, ui: Option<&Ui>) -> bool {
        self.host_switch.is_some() || ui.is_some_and(|ui| !ui.connected && ui.changing)
    }

    fn connect_host(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let alias = match SshAlias::new(name) {
            Ok(alias) => alias,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        self.alias = alias.as_str().to_owned();
        self.switch_to(Destination::Ssh(alias), AttachTarget::Default, window, cx);
    }

    fn reconnect_ssh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        let ui = session.ui();
        if ui.connected && ui.inventory_offline {
            session.reconnect_inventory();
        } else if ui.ssh_offline
            && let Some(destination) = self.destination.clone()
        {
            self.switch_to(
                destination,
                ui.reconnect_target.unwrap_or(AttachTarget::Default),
                window,
                cx,
            );
        }
    }

    fn switch_to(
        &mut self,
        destination: Destination,
        target: AttachTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.window_handle() != self.terminal_window.into() {
            let terminal = self.terminal_window;
            cx.defer(move |cx| {
                let _ = terminal.update(cx, |view, window, cx| {
                    window.activate_window();
                    view.switch_to(destination, target, window, cx);
                });
            });
            return;
        }
        let ui = self.session.as_ref().map(Session::ui);
        if self.connecting(ui.as_ref()) {
            return;
        }
        self.error = None;
        self.menu_open = false;
        self.switcher_open = false;
        if matches!(target, AttachTarget::Default)
            && ui.is_some_and(|ui| ui.can_write())
            && self.destination.as_ref() == Some(&destination)
        {
            self.focus_terminal(window, cx);
            cx.notify();
        } else if self.lifetime.available() {
            window.focus(&self.setup_focus, cx);
            self.connect(destination, target, true, window, cx);
        } else {
            window.focus(&self.setup_focus, cx);
            self.lifetime
                .retire(self.destination.as_ref() != Some(&destination));
            self.host_switch = Some(cx.spawn_in(window, async move |view, cx| {
                loop {
                    let finished = view
                        .update_in(cx, |view, window, cx| {
                            if !view.lifetime.available() {
                                return false;
                            }
                            view.host_switch = None;
                            view.connect(destination.clone(), target.clone(), true, window, cx);
                            true
                        })
                        .unwrap_or(true);
                    if finished {
                        break;
                    }
                    cx.background_executor()
                        .timer(Duration::from_millis(25))
                        .await;
                }
            }));
            cx.notify();
        }
    }

    fn restore_agents(
        &mut self,
        view: ViewSelection,
        pane_only: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ui) = self
            .session
            .as_ref()
            .map(Session::ui)
            .filter(|ui| ui.restore_supported && !ui.changing)
        else {
            return;
        };
        let Some(host_id) = ui.host_id else { return };
        let Some(destination) = self.destination.clone() else {
            return;
        };
        self.switch_to(
            destination,
            AttachTarget::ResumeAgents {
                view,
                host_id,
                pane_only,
                automatic_epoch: None,
            },
            window,
            cx,
        );
    }

    fn connect(
        &mut self,
        destination: Destination,
        target: AttachTarget,
        explicit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.lifetime.available() {
            return;
        }
        let result = Session::start(
            destination.clone(),
            self.agent_deck.clone(),
            self.store.clone(),
            target,
            explicit,
            self.lifetime.parked(),
        );
        let connection = match result {
            Ok(connection) => connection,
            Err(error) => {
                let error = format!("{error:#}");
                self.creation_failed(error.clone());
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.show_connection(destination, connection, window, cx);
    }

    fn show_connection(
        &mut self,
        destination: Destination,
        (session, job, mut prompts, mut activations): Connection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.lifetime
            .replace(job, self.destination.as_ref() != Some(&destination));

        self.restore_picker_shown = false;
        self.error = None;
        self.terminal = None;
        self.selected = None;
        self.tab_bar = tab_bar::TabBar::default();
        if self.destination.as_ref() != Some(&destination) {
            self.preferred_tabs.clear();
            self.preferred_panes.clear();
        }
        let previous = self.destination.replace(destination.clone());
        self.host_lookups.remove(&destination);
        self.lookup_errors.remove(&destination);
        if let Some(known) = self.known_sessions.get_mut(&destination) {
            known.watch = None;
            known.live = false;
        }
        if let Some(previous) = previous.filter(|previous| previous != &destination) {
            self.watch_host(previous, window, cx);
        }
        let changes = session.changes();
        self.session = Some(session);
        self.sync(window, cx);
        self.updates = Some(cx.spawn_in(window, async move |view, cx| {
            watch_on_frames(changes, cx, move |(), window, cx| {
                view.update(cx, |view, cx| view.sync(window, cx)).is_ok()
            })
            .await;
        }));
        self.events = Some(cx.spawn_in(window, async move |view, cx| {
            loop {
                corvid_async::select! {
                    Some(activation) = activations.recv() => {
                        let _ = view.update_in(cx, |view, window, cx| {
                            if activation.reply.is_closed() {
                                return;
                            }
                            if view.session.as_ref().is_some_and(|session| {
                                session.matches_view(activation.view_epoch, activation.revision)
                            }) {
                                if view.session.as_ref().and_then(|session| session.ui().selected) != view.selected {
                                    view.sync(window, cx);
                                }
                                view.focus_from_deck(window, cx, activation.reply);
                            } else {
                                let _ = activation.reply.send(Err(
                                    "The selected Desktop pane changed before it could receive focus.".into(),
                                ));
                            }
                        });
                    }
                    Some(prompt) = prompts.recv() => {
                        let reply = view.update_in(cx, |_, window, cx| prompt.show(window, cx));
                        if let Ok(reply) = reply {
                            reply.await;
                        }
                    }
                    else => break,
                }
            }
        }));
        cx.notify();
    }

    fn watch_host(&mut self, destination: Destination, window: &Window, cx: &mut Context<Self>) {
        let Some(known) = self
            .known_sessions
            .get(&destination)
            .filter(|known| known.watch.is_none())
        else {
            return;
        };
        let Ok(updates) = host_watch::start(known.endpoint.clone(), known.host_id.clone()) else {
            return;
        };
        if self
            .known_sessions
            .values()
            .filter(|known| known.watch.is_some())
            .count()
            >= MAX_BACKGROUND_HOSTS
            && let Some(oldest) = self
                .known_sessions
                .values_mut()
                .filter(|known| known.watch.is_some())
                .min_by_key(|known| known.watch.as_ref().map(|(started, _)| *started))
        {
            oldest.watch = None;
            oldest.live = false;
        }
        let target = destination.clone();
        let task = cx.spawn_in(window, async move |view, cx| {
            watch_on_frames(updates, cx, move |inventory, _, cx| {
                view.update(cx, |view, cx| {
                    if let Some(known) = view.known_sessions.get_mut(&target) {
                        known.live = inventory.is_some();
                        if let Some(inventory) = inventory {
                            known.inventory = Some(inventory);
                        }
                        cx.notify();
                    }
                })
                .is_ok()
            })
            .await;
        });
        let known = self
            .known_sessions
            .get_mut(&destination)
            .expect("known host");
        known.watch = Some((Instant::now(), task));
    }

    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ui) = self.session.as_ref().map(Session::ui) else {
            return;
        };
        self.sync_creation(&ui, window, cx);
        if ui.closed_view {
            self.naming = None;
            self.selected = None;
            self.terminal = None;
        }
        if ui.inventory_ready
            && let (Some(destination), Some(host_id), Some(endpoint), Some(inventory)) =
                (&self.destination, &ui.host_id, &ui.endpoint, &ui.inventory)
            && self
                .known_sessions
                .get(destination)
                .and_then(|known| known.inventory.as_ref())
                .is_none_or(|cached| !Arc::ptr_eq(cached, inventory))
        {
            self.known_sessions.insert(
                destination.clone(),
                KnownSessions {
                    host_id: host_id.clone(),
                    endpoint: endpoint.clone(),
                    inventory: Some(Arc::clone(inventory)),
                    live: true,
                    watch: None,
                },
            );
        }
        if !ui.connected
            && !ui.changing
            && let Some(destination) = &self.destination
        {
            if let (Some(host_id), Some(endpoint)) = (ui.host_id, ui.endpoint) {
                self.known_sessions
                    .entry(destination.clone())
                    .or_insert_with(|| KnownSessions {
                        host_id,
                        endpoint,
                        inventory: None,
                        live: false,
                        watch: None,
                    });
            }
            if ui.ssh_offline {
                if let Some(known) = self.known_sessions.get_mut(destination) {
                    known.watch = None;
                    known.live = false;
                }
            } else {
                self.watch_host(destination.clone(), window, cx);
            }
        }
        if ui.restore_unavailable && ui.selected.is_none() && !self.restore_picker_shown {
            self.restore_picker_shown = true;
            self.show_switcher(window, cx);
        }
        if ui.connected
            && let Some(selected) = ui.selected
            && self.selected.as_ref() != Some(&selected)
        {
            self.preferred_tabs
                .insert(selected.workspace_id.clone(), selected.tab_id.clone());
            self.preferred_panes
                .insert(selected.tab_id.clone(), selected.pane_id.clone());
            let pane = selected.pane_id.clone();
            self.naming = None;
            let session = self.session.clone().expect("connected session");
            let click = self
                .pending_pane_click
                .take()
                .and_then(|(target, cell, time)| {
                    (target == selected && time.elapsed() < Duration::from_secs(2)).then_some(cell)
                });
            self.selected = Some(selected);
            self.terminal = Some(cx.new(|cx| {
                let mut view = TerminalView::new(session, pane, window, cx);
                view.pending_pane_click(click);
                view
            }));
            if !self.switcher_open
                && !self.menu_open
                && self.navigation.is_none()
                && self.creation.is_none()
            {
                self.focus_terminal(window, cx);
            }
        }
        cx.notify();
    }

    fn focus_terminal(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = &self.terminal {
            let focus = terminal.read(cx).focus_handle().clone();
            window.focus(&focus, cx);
        }
    }

    fn select(
        &mut self,
        target: ViewSelection,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_pane_click = None;
        if let Some(session) = &self.session
            && session.ui().connected
        {
            let ui = session.ui();
            if ui.changing {
                return;
            }
            if ui.selected.as_ref() != Some(&target)
                && let Some(inventory) = &ui.inventory
                && inventory.has_writer(&target)
                && let Some(terminal) = inventory
                    .terminals
                    .iter()
                    .find(|terminal| terminal.location.as_ref() == Some(&target))
                && let (Some(destination), Some(host_id)) = (self.destination.clone(), ui.host_id)
            {
                self.switch_to(
                    destination,
                    AttachTarget::Takeover {
                        view: target,
                        host_id,
                        terminal_id: terminal.terminal_id.clone(),
                    },
                    window,
                    cx,
                );
                return;
            }
            session.select(target, open);
            self.focus_terminal(window, cx);
            cx.notify();
            return;
        }
        if let Some(destination) = self.destination.clone() {
            self.connect(
                destination,
                AttachTarget::Pane(target, open),
                false,
                window,
                cx,
            );
        }
    }

    fn open_pane(
        &mut self,
        target: ViewSelection,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if open
            && let Some(destination) = self.destination.clone()
            && let Some(ui) = self.session.as_ref().map(Session::ui)
            && ui.recovery_supported
            && let Some(host_id) = ui.host_id
        {
            self.switch_to(
                destination,
                AttachTarget::OpenPane {
                    view: target,
                    host_id,
                },
                window,
                cx,
            );
        } else {
            self.select(target, open, window, cx);
        }
    }

    fn open_workspace(
        &mut self,
        destination: Destination,
        host_id: String,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.destination.as_ref() == Some(&destination)
            && let Some(ui) = self.session.as_ref().map(Session::ui).filter(Ui::can_write)
        {
            if ui.host_id.as_ref() == Some(&host_id)
                && ui.inventory_ready
                && let Some(inventory) = ui.inventory
                && let Some(workspace) = inventory
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == id)
                && let Some((target, open)) =
                    self.choose_workspace(&destination, &host_id, workspace, &inventory)
            {
                self.open_pane(target, open, window, cx);
            }
            return;
        }
        let target = AttachTarget::Workspace {
            id,
            host_id,
            new_tab: None,
        };
        self.switch_to(destination, target, window, cx);
    }

    fn choose_workspace(
        &self,
        destination: &Destination,
        host_id: &str,
        workspace: &WorkspaceState,
        inventory: &Inventory,
    ) -> Option<(ViewSelection, bool)> {
        let target = destination.saved_target();
        let id = &workspace.workspace_id;
        let remembered = self
            .preferred_tabs
            .get(id)
            .and_then(|tab_id| {
                let view = ViewSelection {
                    workspace_id: id.clone(),
                    tab_id: tab_id.clone(),
                    pane_id: self.preferred_panes.get(tab_id)?.clone(),
                };
                SavedView::new(target.clone(), host_id, &view).ok()
            })
            .or_else(|| self.store.for_workspace(&target, host_id, id));
        if let Some(view) = remembered.and_then(|saved| {
            saved.preferred_pane(workspace, |view| {
                inventory.live(view)
                    && (self.selected.as_ref() == Some(view) || !inventory.has_writer(view))
            })
        }) {
            return Some((view, false));
        }
        self.choose(workspace, workspace.tabs.first()?, inventory)
    }

    fn create(&self, target: Target) {
        if let Some(session) = &self.session {
            session.create(target);
        }
    }

    fn disconnect_others(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone().filter(|session| {
            let ui = session.ui();
            ui.can_write() && ui.session_clients_supported && !ui.changing
        }) else {
            return;
        };
        let revision = session.view_revision();
        let reply = window.prompt(
            PromptLevel::Warning,
            "Disconnect other clients?",
            Some("Applies to the current workspace.\nShells and agents keep running."),
            &["Cancel", "Disconnect"],
            cx,
        );
        cx.spawn(async move |_, _| {
            if reply.await == Ok(1) {
                session.disconnect_others(revision);
            }
        })
        .detach();
    }

    fn close_button(
        id: &'static str,
        title: &'static str,
        target: CloseView,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .flex_none()
            .px(ui_px(6.))
            .rounded_md()
            .text_size(ui_px(15.))
            .text_color(rgb(MUTED))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(BORDER)).text_color(rgb(TEXT)))
            .child("×")
            .on_click(cx.listener(move |view, _, window, cx| {
                cx.stop_propagation();
                view.confirm_close(title, target.clone(), window, cx);
            }))
    }

    fn confirm_close(
        &self,
        title: &str,
        mut target: CloseView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self
            .session
            .clone()
            .filter(|session| session.ui().can_close())
        else {
            return;
        };
        target.expected_view_revision = Some(session.view_revision());
        let end = PromptButton::ok("End terminals");
        let cancel = PromptButton::cancel("Cancel");
        let (buttons, confirm_index) = if target.tab_id.is_some() {
            ([end, cancel], 0)
        } else {
            ([cancel, end], 1)
        };
        let reply = window.prompt(
            PromptLevel::Warning,
            title,
            Some("The selected terminals and their jobs will stop.\nTo keep them running, use Detach or close the window."),
            &buttons,
            cx,
        );
        cx.spawn(async move |_, _| {
            if reply.await == Ok(confirm_index) {
                session.close(target);
            }
        })
        .detach();
    }

    fn choose(
        &self,
        workspace: &WorkspaceState,
        tab: &TabState,
        inventory: &Inventory,
    ) -> Option<(ViewSelection, bool)> {
        let mut panes = tab.nodes.iter().filter_map(|node| match &node.body {
            Some(Node::PaneId(pane)) => Some(ViewSelection {
                workspace_id: workspace.workspace_id.clone(),
                tab_id: tab.tab_id.clone(),
                pane_id: pane.clone(),
            }),
            _ => None,
        });
        let first = panes.next()?;
        let preferred = self.preferred_panes.get(&tab.tab_id);
        let mut live: Vec<_> = std::iter::once(first.clone())
            .chain(panes)
            .filter(|pane| inventory.live(pane))
            .collect();
        let available = |pane: &ViewSelection| {
            self.selected.as_ref() == Some(pane) || !inventory.has_writer(pane)
        };
        if let Some(index) = live
            .iter()
            .position(|pane| preferred == Some(&pane.pane_id) && available(pane))
            .or_else(|| live.iter().position(available))
        {
            return Some((live.swap_remove(index), false));
        }
        Some(
            live.into_iter()
                .next()
                .map_or((first, true), |pane| (pane, false)),
        )
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.creation.is_some() {
            self.creation_key(event, window, cx);
            return;
        }
        if appearance::terminal_shortcut(event, window, cx) {
            return;
        }
        if self.naming.is_some() {
            self.naming_key(event, window, cx);
            return;
        }
        if self.switcher_open {
            self.switcher_key(event, window, cx);
            return;
        }
        if self.menu_open {
            self.menu_key(event, window, cx);
            return;
        }
        self.navigation_key(event, window, cx);
    }

    fn cycle_tab(&mut self, reverse: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some((selected, inventory)) = self.navigation_target() else {
            return;
        };
        let Some(workspace) = inventory
            .workspaces
            .iter()
            .find(|ws| ws.workspace_id == selected.workspace_id)
        else {
            return;
        };
        let Some(index) = workspace
            .tabs
            .iter()
            .position(|tab| tab.tab_id == selected.tab_id)
        else {
            return;
        };
        let len = workspace.tabs.len();
        let tab = &workspace.tabs[(index + if reverse { len - 1 } else { 1 }) % len];
        if let Some((target, open)) = self.choose(workspace, tab, &inventory) {
            self.open_pane(target, open, window, cx);
        }
    }

    fn terminal_placeholder(&self, ui: Option<&Ui>, cx: &mut Context<Self>) -> Div {
        let destination = self.destination.clone().unwrap_or(Destination::Local);
        let changing = self.connecting(ui);
        let message = self
            .error
            .as_deref()
            .or_else(|| ui.map(|ui| ui.message.as_str()));
        div()
            .flex()
            .flex_1()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(ui_px(12.))
            .p(ui_px(18.))
            .child(
                div()
                    .text_size(ui_px(19.))
                    .text_color(rgb(TEXT))
                    .child(format!("Terminal · {}", destination.label())),
            )
            .when_some(message, |panel, message| {
                panel.child(
                    div()
                        .max_w(ui_px(540.))
                        .text_center()
                        .text_size(ui_px(13.))
                        .text_color(rgb(MUTED))
                        .child(message.to_owned()),
                )
            })
            .when(!changing && !ui.is_some_and(|ui| ui.ssh_offline), |panel| {
                panel.child(
                    button("open-terminal", "Open Terminal", true).on_click(cx.listener(
                        move |view, _, window, cx| {
                            view.switch_to(destination.clone(), AttachTarget::Default, window, cx)
                        },
                    )),
                )
            })
    }

    fn panes(
        &self,
        tab: &TabState,
        index: usize,
        ui: &Ui,
        inventory: &Inventory,
        ordinal: &mut usize,
        cx: &mut Context<Self>,
    ) -> Div {
        let region = div().flex().flex_1().min_w_0().min_h_0().overflow_hidden();
        match tab.nodes.get(index).and_then(|node| node.body.as_ref()) {
            Some(Node::Split(split)) => region
                .gap(ui_px(1.))
                .when(split.axis == SplitAxis::Vertical as i32, |region| {
                    region.flex_col()
                })
                .children(
                    split
                        .children
                        .iter()
                        .map(|&child| self.panes(tab, child as usize, ui, inventory, ordinal, cx)),
                ),
            Some(Node::PaneId(pane)) => {
                let selected = ui.selected.as_ref().expect("pane tree has a selected tab");
                *ordinal += 1;
                let target = ViewSelection {
                    pane_id: pane.clone(),
                    ..selected.clone()
                };
                let active = ui.connected && target == *selected;
                let live = inventory.live(&target);
                let title = tab.nodes[index].title.clone();
                let title = if title.is_empty() {
                    format!("Pane {}", *ordinal)
                } else {
                    title
                };
                let preview_session = self
                    .session
                    .clone()
                    .filter(|_| ui.connected && live && !active);
                region
                    .flex_col()
                    .border_1()
                    .border_color(rgb(if self.pane_highlight(pane) {
                        NAVIGATION_HIGHLIGHT
                    } else if active {
                        ACTIVE
                    } else {
                        BORDER
                    }))
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .px(ui_px(9.))
                            .py(ui_px(4.))
                            .bg(rgb(if active { ACTIVE_FILL } else { PANEL }))
                            .text_color(rgb(if active { ACTIVE } else { MUTED }))
                            .text_size(ui_px(12.))
                            .child(div().flex_1().min_w_0().truncate().child(title))
                            .when(ui.connected && ui.close_supported, |header| {
                                header.child(
                                    div()
                                        .id(SharedString::from(format!("pane-header-{pane}")))
                                        .child(Self::close_button(
                                            "close-pane",
                                            "Close this pane?",
                                            CloseView {
                                                workspace_id: target.workspace_id.clone(),
                                                tab_id: Some(target.tab_id.clone()),
                                                pane_id: Some(target.pane_id.clone()),
                                                ..Default::default()
                                            },
                                            cx,
                                        )),
                                )
                            }),
                    )
                    .when(active, |region| {
                        region.child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .children(self.terminal.iter().cloned()),
                        )
                    })
                    .when_some(preview_session.clone(), |region, session| {
                        let target = target.clone();
                        let surface = Rc::new(std::cell::Cell::new(None));
                        region.child(
                            div()
                                .id(SharedString::from(format!("preview-{pane}")))
                                .flex_1()
                                .min_h_0()
                                .overflow_hidden()
                                .cursor_pointer()
                                .child(preview(session, pane.clone(), Rc::clone(&surface)))
                                .on_click(cx.listener(
                                    move |view, event: &gpui::ClickEvent, window, cx| {
                                        let cell = if event.modifiers().modified() {
                                            None
                                        } else {
                                            event.mouse_position().zip(surface.get()).map(
                                                |(point, bounds)| {
                                                    crate::terminal_view::preview_cell(
                                                        bounds, point, window, cx,
                                                    )
                                                },
                                            )
                                        };
                                        view.select(target.clone(), false, window, cx);
                                        view.pending_pane_click =
                                            cell.map(|cell| (target.clone(), cell, Instant::now()));
                                    },
                                )),
                        )
                    })
                    .when(!active && preview_session.is_none(), |region| {
                        let restore_target = target.clone();
                        let restore_error = ui
                            .restore_results
                            .iter()
                            .find(|result| result.location.as_ref() == Some(&target))
                            .and_then(|result| result.error.as_ref())
                            .map(|error| error.message.clone());
                        region.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(ui_px(10.))
                                .p(ui_px(12.))
                                .flex_1()
                                .items_center()
                                .justify_center()
                                .when_some(restore_error, |panel, error| {
                                    panel.child(
                                        div()
                                            .text_size(ui_px(12.))
                                            .text_color(rgb(MUTED))
                                            .child(error),
                                    )
                                })
                                .when(!live && ui.restore_supported && !ui.changing, |panel| {
                                    panel.child(
                                        button(
                                            SharedString::from(format!("resume-{pane}")),
                                            "Restore / retry agent session",
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(move |view, _, window, cx| {
                                                view.restore_agents(
                                                    restore_target.clone(),
                                                    true,
                                                    window,
                                                    cx,
                                                )
                                            }),
                                        ),
                                    )
                                })
                                .child(
                                    button(
                                        SharedString::from(pane.clone()),
                                        if live { "Focus pane" } else { "Open shell" },
                                        false,
                                    )
                                    .on_click(cx.listener(
                                        move |view, _, window, cx| {
                                            view.select(target.clone(), !live, window, cx)
                                        },
                                    )),
                                ),
                        )
                    })
            }
            None => region,
        }
    }

    fn terminal_layout(&self, ui: &Ui, inventory: &Inventory, cx: &mut Context<Self>) -> Div {
        if self.zoom_pane && ui.connected {
            return div()
                .flex()
                .flex_1()
                .min_h_0()
                .children(self.terminal.iter().cloned());
        }
        let Some(selected) = &ui.selected else {
            return self.terminal_placeholder(Some(ui), cx);
        };
        let workspace = inventory
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == selected.workspace_id);
        let tab = workspace.and_then(|workspace| {
            workspace
                .tabs
                .iter()
                .find(|tab| tab.tab_id == selected.tab_id)
        });
        if tab.is_some_and(|tab| {
            tab.nodes
                .iter()
                .filter(|node| matches!(node.body, Some(Node::PaneId(_))))
                .count()
                == 1
        }) && ui.connected
        {
            return div()
                .flex()
                .flex_1()
                .min_h_0()
                .children(self.terminal.iter().cloned());
        }
        let body = match tab {
            Some(tab) if tab.nodes.iter().any(|node| matches!(&node.body, Some(Node::PaneId(pane)) if *pane == selected.pane_id)) =>
                self.panes(tab, 0, ui, inventory, &mut 0, cx),
            _ => div().flex_1().children(self.terminal.iter().cloned()),
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(div().flex().flex_1().min_h_0().child(body))
    }
}

fn button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    selected: bool,
) -> Stateful<Div> {
    button_frame(id, selected).child(label.into())
}

fn button_frame(id: impl Into<SharedString>, selected: bool) -> Stateful<Div> {
    div()
        .id(id.into())
        .focusable()
        .tab_index(0)
        .focus(|style| style.bg(rgb(ACTIVE_FILL)).text_color(rgb(ACTIVE)))
        .flex_none()
        .px(ui_px(10.))
        .py(ui_px(5.))
        .rounded_md()
        .bg(rgb(if selected { ACTIVE_FILL } else { PANEL }))
        .text_color(rgb(if selected { ACTIVE } else { MUTED }))
        .text_size(ui_px(13.))
        .whitespace_nowrap()
        .cursor_pointer()
        .hover(|style| style.text_color(rgb(TEXT)))
}

fn alias_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"._-".contains(&byte)
}
