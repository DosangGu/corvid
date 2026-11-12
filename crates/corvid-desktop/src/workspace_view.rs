use std::cell::RefCell;
mod chrome;
mod connections;
mod host_watch;
mod management;
mod naming;
mod switcher;
mod update;
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
    AppContext, Context, Div, Entity, FocusHandle, IntoElement, KeyDownEvent, PromptLevel,
    SharedString, Stateful, Task, Window, WindowHandle, div, prelude::*, px, rgb,
};

use crate::agent_deck;
use crate::ssh_hosts;
use crate::terminal_session::{AttachTarget, Destination, Inventory, Job, Session, Ui};
use crate::terminal_view::{TerminalView, preview};

const BACKGROUND: u32 = 0x101318;
const PANEL: u32 = 0x191c23;
const BORDER: u32 = 0x30343f;
const TEXT: u32 = 0xe3e8ee;
const MUTED: u32 = 0xa4adbc;
const ACTIVE: u32 = 0x83dec0;
const ACTIVE_FILL: u32 = 0x20332f;
const MAX_BACKGROUND_HOSTS: usize = 8;

#[derive(Clone, Default)]
pub struct Lifetime(Rc<RefCell<Option<Job>>>);

impl Lifetime {
    fn replace(&self, job: Job) {
        if let Some(old) = self.0.replace(Some(job)) {
            old.join();
        }
    }

    fn available(&self) -> bool {
        self.0.borrow().as_ref().is_none_or(Job::finished)
    }

    fn stop(&self) {
        if let Some(job) = self.0.borrow_mut().as_mut() {
            job.stop();
        }
    }

    pub fn shutdown(&self) {
        self.stop();
        if let Some(job) = self.0.take() {
            job.join();
        }
    }
}

pub struct WorkspaceView {
    naming: Option<naming::NameDraft>,
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
    terminal: Option<Entity<TerminalView>>,
    selected: Option<ViewSelection>,
    preferred_tabs: HashMap<String, String>,
    preferred_panes: HashMap<String, String>,
    updates: Option<Task<()>>,
    error: Option<String>,
    deck_focus: u64,
    agent_deck: agent_deck::Settings,
    switcher_open: bool,
    switcher_query: String,
    switcher_index: usize,
    terminal_window: WindowHandle<Self>,
    management_window: Option<WindowHandle<management::ManagementView>>,
    management_page: management::Page,
    focus_mode: bool,
    zoom_pane: bool,
    observe_accept_writer: bool,
    menu_open: bool,
    menu_create: bool,
    menu_index: usize,
    release_update: Option<Task<()>>,
    update: update::State,
}

struct KnownSessions {
    host_id: String,
    endpoint: ControlEndpoint,
    inventory: Arc<Inventory>,
    live: bool,
    watch: Option<(Instant, Task<()>)>,
}

impl WorkspaceView {
    pub fn new(
        destination: Option<Destination>,
        lifetime: Lifetime,
        agent_deck: agent_deck::Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let setup_focus = cx.focus_handle();
        window.focus(&setup_focus);
        let mut view = Self {
            naming: None,
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
            terminal: None,
            selected: None,
            preferred_tabs: HashMap::new(),
            preferred_panes: HashMap::new(),
            updates: None,
            error: None,
            deck_focus: 0,
            agent_deck,
            switcher_open: false,
            switcher_query: String::new(),
            switcher_index: 0,
            terminal_window: window.window_handle().downcast().expect("workspace window"),
            management_window: None,
            management_page: management::Page::Connections,
            focus_mode: false,
            zoom_pane: false,
            observe_accept_writer: false,
            menu_open: false,
            menu_create: false,
            menu_index: 0,
            release_update: None,
            update: update::State::default(),
        };
        cx.on_release(|view, _| view.lifetime.stop()).detach();
        view.switch_to(
            destination.unwrap_or(Destination::Local),
            AttachTarget::Default,
            window,
            cx,
        );
        view
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
        let scan = cx.background_executor().spawn(async {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(WslDistribution::list())
        });
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
            window.focus(&self.setup_focus);
            self.connect(destination, target, true, window, cx);
        } else {
            window.focus(&self.setup_focus);
            self.lifetime.stop();
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
            self.agent_deck.state.clone(),
            target,
            explicit,
        );
        let (session, job, mut prompts) = match result {
            Ok(connection) => connection,
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                cx.notify();
                return;
            }
        };
        self.lifetime.replace(job);
        self.error = None;
        self.terminal = None;
        self.selected = None;
        self.deck_focus = 0;
        if self.destination.as_ref() != Some(&destination) {
            self.preferred_tabs.clear();
            self.preferred_panes.clear();
        }
        let previous = self.destination.replace(destination.clone());
        if let Some(known) = self.known_sessions.get_mut(&destination) {
            known.watch = None;
            known.live = false;
        }
        if let Some(previous) = previous.filter(|previous| previous != &destination) {
            self.watch_host(previous, cx);
        }
        let mut changes = session.changes();
        self.session = Some(session);
        self.updates = Some(cx.spawn_in(window, async move |view, cx| {
            loop {
                if view
                    .update_in(cx, |view, window, cx| view.sync(window, cx))
                    .is_err()
                {
                    break;
                }
                tokio::select! {
                    change = changes.changed() => {
                        if change.is_err() {
                            break;
                        }
                    }
                    Some(prompt) = prompts.recv() => {
                        let reply = view.update_in(cx, |_, window, cx| {
                            let buttons = match prompt.action {
                                Some(action) => vec!["Cancel", action],
                                None => vec!["OK"],
                            };
                            window.prompt(
                                PromptLevel::Warning,
                                prompt.title,
                                Some(&prompt.message),
                                &buttons,
                                cx,
                            )
                        });
                        if let Ok(reply) = reply {
                            let _ = prompt.decision.send(reply.await == Ok(1));
                        }
                    }
                }
            }
        }));
        cx.notify();
    }

    fn watch_host(&mut self, destination: Destination, cx: &mut Context<Self>) {
        let Some(known) = self
            .known_sessions
            .get(&destination)
            .filter(|known| known.watch.is_none())
        else {
            return;
        };
        let Ok(mut updates) = host_watch::start(known.endpoint.clone(), known.host_id.clone())
        else {
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
        let task = cx.spawn(async move |view, cx| {
            while updates.changed().await.is_ok() {
                let inventory = updates.borrow_and_update().clone();
                if view
                    .update(cx, |view, cx| {
                        if let Some(known) = view.known_sessions.get_mut(&target) {
                            known.live = inventory.is_some();
                            if let Some(inventory) = inventory {
                                known.inventory = inventory;
                            }
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let known = self
            .known_sessions
            .get_mut(&destination)
            .expect("known host");
        known.live = false;
        known.watch = Some((Instant::now(), task));
    }

    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ui) = self.session.as_ref().map(Session::ui) else {
            return;
        };
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
                .is_none_or(|known| !Arc::ptr_eq(&known.inventory, inventory))
        {
            self.known_sessions.insert(
                destination.clone(),
                KnownSessions {
                    host_id: host_id.clone(),
                    endpoint: endpoint.clone(),
                    inventory: Arc::clone(inventory),
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
                        inventory: Arc::default(),
                        live: false,
                        watch: None,
                    });
            }
            self.watch_host(destination.clone(), cx);
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
            self.selected = Some(selected);
            self.terminal = Some(cx.new(|cx| TerminalView::new(session, pane, cx)));
            if !self.switcher_open && !self.menu_open {
                self.focus_terminal(window, cx);
            }
        }
        if self.deck_focus != ui.deck_focus {
            self.naming = None;
            self.deck_focus = ui.deck_focus;
            window.activate_window();
            self.focus_terminal(window, cx);
        }
        cx.notify();
    }

    fn focus_terminal(&self, window: &mut Window, cx: &Context<Self>) {
        if let Some(terminal) = &self.terminal {
            terminal.read(cx).focus(window);
        }
    }

    fn select(
        &mut self,
        target: ViewSelection,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = &self.session
            && session.ui().connected
        {
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

    fn open_workspace(
        &mut self,
        destination: Destination,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.destination.as_ref() == Some(&destination)
            && let Some(ui) = self.session.as_ref().map(Session::ui).filter(Ui::can_write)
        {
            if ui.inventory_ready
                && let Some(inventory) = ui.inventory
                && let Some(workspace) = inventory
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == id)
                && let Some(tab) = workspace
                    .tabs
                    .iter()
                    .find(|tab| self.preferred_tabs.get(&id) == Some(&tab.tab_id))
                    .or_else(|| workspace.tabs.first())
                && let Some((target, open)) = self.choose(workspace, tab, &inventory)
            {
                self.select(target, open, window, cx);
            }
            return;
        }
        if let Some(known) = self.known_sessions.get(&destination) {
            let target = AttachTarget::Workspace {
                id,
                host_id: known.host_id.clone(),
                new_tab: None,
            };
            self.switch_to(destination, target, window, cx);
        }
    }

    fn create(&self, target: Target) {
        if let Some(session) = &self.session {
            session.create(target);
        }
    }

    fn observe(
        &mut self,
        destination: Destination,
        workspace: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let known = self.known_sessions.get(&destination);
        let view = workspace.and_then(|id| {
            self.selected
                .as_ref()
                .filter(|selected| {
                    self.destination.as_ref() == Some(&destination) && selected.workspace_id == id
                })
                .cloned()
                .or_else(|| known?.inventory.observation_target(id))
        });
        if workspace.is_some() && view.is_none() {
            return;
        }
        let target = AttachTarget::Observe {
            view,
            host_id: known.map(|known| known.host_id.clone()),
            accept_writer: self.observe_accept_writer,
        };
        self.switch_to(destination, target, window, cx);
    }

    fn take_over(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ui) = self
            .session
            .as_ref()
            .map(Session::ui)
            .filter(|ui| ui.connected && ui.read_only && ui.session_clients_supported)
        else {
            return;
        };
        if let (Some(destination), Some(view), Some(host_id), Some(terminal_id)) = (
            self.destination.clone(),
            ui.selected,
            ui.host_id,
            ui.terminal_id,
        ) {
            self.switch_to(
                destination,
                AttachTarget::Takeover {
                    view,
                    host_id,
                    terminal_id,
                },
                window,
                cx,
            );
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
            .px(px(6.))
            .rounded_md()
            .text_size(px(15.))
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
        title: &'static str,
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
        let reply = window.prompt(
            PromptLevel::Warning,
            title,
            Some("The selected terminals and their jobs will stop.\nTo keep them running, use Detach or close the window."),
            &["Cancel", "End terminals"],
            cx,
        );
        cx.spawn(async move |_, _| {
            if reply.await == Ok(1) {
                session.close(target);
            }
        })
        .detach();
    }

    fn new_workspace(&self, count: usize) {
        self.create(Target::WorkspaceLabel(format!("Workspace {}", count + 1)));
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
        if self.naming.is_some() {
            self.naming_key(event, window, cx);
            return;
        }
        let key = &event.keystroke;
        let ui = self.session.as_ref().map(Session::ui);
        if self.switcher_open {
            self.switcher_key(event, window, cx);
            return;
        }
        if self.menu_open {
            self.menu_key(event, window, cx);
            return;
        }
        let modifiers = key.modifiers;
        if modifiers.control
            && !modifiers.platform
            && key.key.eq_ignore_ascii_case("r")
            && (modifiers.shift ^ modifiers.alt)
        {
            if !event.is_held {
                self.open_naming(modifiers.shift, window, cx);
            }
            cx.stop_propagation();
            return;
        }
        if !modifiers.control || modifiers.alt || modifiers.platform {
            return;
        }
        let name = key.key.to_ascii_lowercase();
        let shortcut = (modifiers.shift, name.as_str());
        if event.is_held
            && matches!(
                shortcut,
                (true, "n" | "t" | "d" | "e" | "p" | "f" | "z") | (_, "tab" | ",")
            )
        {
            cx.stop_propagation();
            return;
        }
        match shortcut {
            (true, "p") => self.toggle_switcher(window, cx),
            (false, ",") => self.open_management(management::Page::Connections, window, cx),
            (true, "f") => {
                self.focus_mode = !self.focus_mode;
                cx.notify();
            }
            (true, "z") => {
                self.zoom_pane = !self.zoom_pane;
                cx.notify();
            }
            _ => {
                self.workspace_shortcut(shortcut, ui, window, cx);
                return;
            }
        }
        cx.stop_propagation();
    }

    fn workspace_shortcut(
        &mut self,
        shortcut: (bool, &str),
        ui: Option<Ui>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !ui.as_ref().is_some_and(|ui| ui.can_write() && !ui.changing) {
            return;
        }
        let Some(inventory) = ui.and_then(|ui| ui.inventory) else {
            return;
        };
        let workspace = self.selected.as_ref().and_then(|selected| {
            inventory
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == selected.workspace_id)
        });
        match shortcut {
            (true, "n") => self.new_workspace(inventory.workspaces.len()),
            (true, "t") => self.create(Target::TabLabel(format!(
                "Shell {}",
                workspace.map_or(1, |workspace| workspace.tabs.len() + 1)
            ))),
            (true, "d") => self.create(Target::SplitAxis(SplitAxis::Horizontal as i32)),
            (true, "e") => self.create(Target::SplitAxis(SplitAxis::Vertical as i32)),
            (reverse, "tab") => {
                let Some(workspace) = workspace else { return };
                let index = workspace
                    .tabs
                    .iter()
                    .position(|tab| {
                        self.selected
                            .as_ref()
                            .is_some_and(|selected| tab.tab_id == selected.tab_id)
                    })
                    .unwrap_or(0);
                let len = workspace.tabs.len();
                if len == 0 {
                    return;
                }
                let tab = &workspace.tabs[(index + if reverse { len - 1 } else { 1 }) % len];
                if let Some((target, open)) = self.choose(workspace, tab, &inventory) {
                    self.select(target, open, window, cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
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
            .gap(px(12.))
            .p(px(18.))
            .child(
                div()
                    .text_size(px(19.))
                    .text_color(rgb(TEXT))
                    .child(format!("Terminal · {}", destination.label())),
            )
            .when_some(message, |panel, message| {
                panel.child(
                    div()
                        .max_w(px(540.))
                        .text_center()
                        .text_size(px(13.))
                        .text_color(rgb(MUTED))
                        .child(message.to_owned()),
                )
            })
            .when(!changing, |panel| {
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
                .gap(px(1.))
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
                let observed = target.clone();
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
                    .border_color(rgb(if active { ACTIVE } else { BORDER }))
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .px(px(9.))
                            .py(px(4.))
                            .bg(rgb(if active { ACTIVE_FILL } else { PANEL }))
                            .text_color(rgb(if active { ACTIVE } else { MUTED }))
                            .text_size(px(12.))
                            .child(div().flex_1().min_w_0().truncate().child(title))
                            .when(
                                ui.connected && !active && inventory.has_writer(&target),
                                |header| {
                                    header.child(button("observe-pane", "Observe", false).on_click(
                                        cx.listener(move |view, _, window, cx| {
                                            if let Some(destination) = view.destination.clone() {
                                                view.switch_to(
                                                    destination,
                                                    AttachTarget::Observe {
                                                        view: Some(observed.clone()),
                                                        host_id: view.session.as_ref().and_then(
                                                            |session| session.ui().host_id,
                                                        ),
                                                        accept_writer: view.observe_accept_writer,
                                                    },
                                                    window,
                                                    cx,
                                                );
                                            }
                                        }),
                                    ))
                                },
                            )
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
                        region.child(
                            div()
                                .id(SharedString::from(format!("preview-{pane}")))
                                .flex_1()
                                .min_h_0()
                                .p(px(2.))
                                .overflow_hidden()
                                .cursor_pointer()
                                .child(preview(session, pane.clone()))
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.select(target.clone(), false, window, cx)
                                })),
                        )
                    })
                    .when(!active && preview_session.is_none(), |region| {
                        region.child(
                            div().flex().flex_1().items_center().justify_center().child(
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
        if (ui.read_only || self.zoom_pane) && ui.connected {
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
    div()
        .id(id.into())
        .flex_none()
        .px(px(10.))
        .py(px(5.))
        .rounded_md()
        .bg(rgb(if selected { ACTIVE_FILL } else { PANEL }))
        .text_color(rgb(if selected { ACTIVE } else { MUTED }))
        .text_size(px(13.))
        .whitespace_nowrap()
        .cursor_pointer()
        .hover(|style| style.text_color(rgb(TEXT)))
        .child(label.into())
}

fn alias_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"._-".contains(&byte)
}
