use std::cell::RefCell;
mod update;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use corvid_client::SshAlias;
#[cfg(windows)]
use corvid_client::WslDistribution;
use corvid_protocol::wire::create_view::Target;
use corvid_protocol::wire::layout_node_state::Body as Node;
use corvid_protocol::wire::{CloseView, SplitAxis, TabState, ViewSelection, WorkspaceState};
use gpui::{
    AppContext, Context, Div, Entity, FocusHandle, IntoElement, KeyDownEvent, PromptLevel,
    SharedString, Stateful, Task, Window, div, prelude::*, px, rgb,
};

use crate::slot_board;
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
    lifetime: Lifetime,
    alias: String,
    setup_focus: FocusHandle,
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
    board_focus: u64,
    slot_board: slot_board::Settings,
    connections_open: bool,
    plugins_open: bool,
    help_open: bool,
    release_update: Option<Task<()>>,
    update_status: String,
}

struct KnownSessions {
    host_id: String,
    inventory: Arc<Inventory>,
}

impl WorkspaceView {
    pub fn new(
        destination: Option<Destination>,
        lifetime: Lifetime,
        slot_board: slot_board::Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let setup_focus = cx.focus_handle();
        window.focus(&setup_focus);
        let mut view = Self {
            lifetime,
            alias: match &destination {
                Some(Destination::Ssh(alias)) => alias.as_str().to_owned(),
                _ => String::new(),
            },
            setup_focus,
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
            board_focus: 0,
            slot_board,
            connections_open: false,
            plugins_open: false,
            help_open: false,
            release_update: None,
            update_status: String::new(),
        };
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
        let ui = self.session.as_ref().map(Session::ui);
        if self.connecting(ui.as_ref()) {
            return;
        }
        self.error = None;
        self.plugins_open = false;
        if matches!(target, AttachTarget::Default)
            && ui.is_some_and(|ui| ui.connected)
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
            self.slot_board.state.clone(),
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
        self.board_focus = 0;
        if self.destination.as_ref() != Some(&destination) {
            self.preferred_tabs.clear();
            self.preferred_panes.clear();
        }
        self.destination = Some(destination);
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

    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        let ui = session.ui();
        if ui.closed_view
            && let Some(destination) = &self.destination
        {
            self.known_sessions.remove(destination);
            self.selected = None;
            self.terminal = None;
        }
        if ui.inventory_ready
            && let (Some(destination), Some(host_id), Some(inventory)) =
                (&self.destination, &ui.host_id, &ui.inventory)
            && self
                .known_sessions
                .get(destination)
                .is_none_or(|known| !Arc::ptr_eq(&known.inventory, inventory))
        {
            self.known_sessions.insert(
                destination.clone(),
                KnownSessions {
                    host_id: host_id.clone(),
                    inventory: Arc::clone(inventory),
                },
            );
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
            let session = session.clone();
            self.selected = Some(selected);
            self.terminal = Some(cx.new(|cx| TerminalView::new(session, pane, cx)));
            if !self.alias_focus.is_focused(window) {
                self.focus_terminal(window, cx);
            }
        }
        if self.board_focus != ui.board_focus {
            self.board_focus = ui.board_focus;
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
            && let Some(ui) = self
                .session
                .as_ref()
                .map(Session::ui)
                .filter(|ui| ui.connected)
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
            };
            self.switch_to(destination, target, window, cx);
        }
    }

    fn create(&self, target: Target) {
        if let Some(session) = &self.session {
            session.create(target);
        }
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
                let Some(session) = view
                    .session
                    .clone()
                    .filter(|session| session.ui().can_close())
                else {
                    return;
                };
                let mut target = target.clone();
                target.expected_view_revision = Some(session.view_revision());
                let reply = window.prompt(
                    PromptLevel::Warning,
                    title,
                    Some("The affected shells and their attached jobs will stop."),
                    &["Cancel", "Close"],
                    cx,
                );
                cx.spawn(async move |_, _| {
                    if reply.await == Ok(1) {
                        session.close(target);
                    }
                })
                .detach();
            }))
    }

    fn new_workspace(&self, count: usize) {
        self.create(Target::WorkspaceLabel(format!("Session {}", count + 1)));
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
        if let Some(index) = live
            .iter()
            .position(|pane| preferred == Some(&pane.pane_id))
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
        let key = &event.keystroke;
        let ui = self.session.as_ref().map(Session::ui);
        let editing_alias = self.alias_focus.is_focused(window);
        if self.connections_open
            && key.key == "escape"
            && (editing_alias || self.setup_focus.is_focused(window))
        {
            self.connections_open = false;
            self.focus_terminal(window, cx);
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if self.connections_open && editing_alias {
            if self.connecting(ui.as_ref()) {
                return;
            } else if key.key == "enter" {
                self.connect_host(self.alias.clone(), window, cx);
            } else if key.key == "backspace" {
                self.alias.pop();
            } else if key.modifiers.control && key.key == "v" {
                if let Some(value) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    let value = value.trim();
                    if value.len() <= 253 && value.bytes().all(alias_byte) {
                        self.alias = value.to_owned();
                    }
                }
            } else if !key.modifiers.control && !key.modifiers.alt && !key.modifiers.platform {
                let value = key.key_char.as_deref().unwrap_or(&key.key);
                if value.len() == 1 && self.alias.len() < 253 && value.bytes().all(alias_byte) {
                    self.alias.push_str(value);
                }
            } else {
                return;
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if !ui.as_ref().is_some_and(|ui| ui.connected) {
            return;
        }
        let modifiers = key.modifiers;
        if !modifiers.control || modifiers.alt || modifiers.platform {
            return;
        }
        let name = key.key.to_ascii_lowercase();
        let shortcut = (modifiers.shift, name.as_str());
        if event.is_held && matches!(shortcut, (true, "n" | "t" | "d" | "e") | (_, "tab")) {
            cx.stop_propagation();
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
                let tab = &workspace.tabs[(index + if reverse { len - 1 } else { 1 }) % len];
                if let Some((target, open)) = self.choose(workspace, tab, &inventory) {
                    self.select(target, open, window, cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn toolbar(&self, connected: bool, workspace: Option<&str>, cx: &mut Context<Self>) -> Div {
        let location = match self.destination.as_ref() {
            Some(Destination::Ssh(alias)) => format!("SSH · {}", alias.as_str()),
            #[cfg(windows)]
            Some(Destination::Wsl(distribution)) => format!("WSL · {}", distribution.as_str()),
            _ => "Local".into(),
        };
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .p(px(10.))
            .bg(rgb(PANEL))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex_none()
                            .text_color(rgb(TEXT))
                            .child("Corvid Desktop"),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child(location),
                    )
                    .when_some(workspace, |title, workspace| {
                        title.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.))
                                .text_color(rgb(TEXT))
                                .child(format!("/ {workspace}")),
                        )
                    }),
            )
            .child(
                button("connections", "Sessions", self.connections_open).on_click(cx.listener(
                    |view, _, window, cx| {
                        view.connections_open = !view.connections_open;
                        if view.connections_open {
                            view.plugins_open = false;
                            window.focus(&view.setup_focus);
                            view.refresh_hosts(cx);
                            #[cfg(windows)]
                            view.refresh_wsl(cx);
                        } else {
                            view.focus_terminal(window, cx);
                        }
                        cx.notify();
                    },
                )),
            )
            .child(
                button("plugins", "Plugins", self.plugins_open).on_click(cx.listener(
                    |view, _, window, cx| {
                        view.plugins_open = !view.plugins_open;
                        if view.plugins_open {
                            view.connections_open = false;
                        }
                        if !view.plugins_open {
                            view.focus_terminal(window, cx);
                        } else {
                            window.focus(&view.setup_focus);
                        }
                        cx.notify();
                    },
                )),
            )
            .when(connected, |bar| {
                bar.child(button("detach", "Detach", false).on_click(cx.listener(
                    |view, _, window, cx| {
                        view.lifetime.stop();
                        window.focus(&view.setup_focus);
                        cx.notify();
                    },
                )))
            })
            .child(button("help", "Help", self.help_open).on_click(cx.listener(
                |view, _, _, cx| {
                    view.help_open = !view.help_open;
                    cx.notify();
                },
            )))
    }

    fn plugins_menu(&self, connected: bool, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .justify_end()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .pb(px(6.))
            .bg(rgb(PANEL))
            .when(connected, |menu| {
                menu.child(
                    button("agent-board", "Agent Board", false).on_click(cx.listener(
                        |view, _, window, cx| {
                            if let Some(session) = &view.session {
                                session.open_slot_board(view.slot_board.clone());
                            }
                            view.plugins_open = false;
                            view.focus_terminal(window, cx);
                            cx.notify();
                        },
                    )),
                )
            })
            .when(!connected, |menu| {
                menu.child(
                    div()
                        .text_size(px(13.))
                        .text_color(rgb(MUTED))
                        .child("Open a Corvid terminal to use Agent Board."),
                )
            })
    }

    fn connections_panel(&self, ui: Option<&Ui>, cx: &mut Context<Self>) -> Div {
        let changing = self.connecting(ui);
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(340.))
            .min_h_0()
            .gap(px(12.))
            .p(px(14.))
            .bg(rgb(PANEL))
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .text_color(rgb(TEXT))
                    .text_size(px(19.))
                    .child("Sessions"),
            )
            .child(
                div()
                    .id("connection-tree")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap(px(12.))
                    .overflow_y_scroll()
                    .child(self.connection_branch(&Destination::Local, ui, changing, cx))
                    .when(cfg!(windows), |tree| {
                        #[cfg(windows)]
                        let tree = tree.child(self.wsl_list(ui, changing, cx));
                        tree
                    })
                    .child(self.host_list(ui, changing, cx)),
            )
            .when_some(self.error.clone(), |panel, error| {
                panel.child(div().text_size(px(13.)).text_color(rgb(MUTED)).child(error))
            })
            .child(
                div()
                    .flex()
                    .flex_none()
                    .gap(px(8.))
                    .items_center()
                    .child(
                        div()
                            .id("ssh-alias")
                            .track_focus(&self.alias_focus)
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .p(px(8.))
                            .rounded_md()
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .text_color(rgb(TEXT))
                            .child(if self.alias.is_empty() {
                                "OpenSSH alias".into()
                            } else {
                                self.alias.clone()
                            })
                            .on_click(
                                cx.listener(|view, _, window, _| window.focus(&view.alias_focus)),
                            ),
                    )
                    .child(
                        button(
                            "connect",
                            if changing { "Connecting…" } else { "Connect" },
                            true,
                        )
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.connect_host(view.alias.clone(), window, cx)
                        })),
                    ),
            )
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

    fn host_list(&self, ui: Option<&Ui>, changing: bool, cx: &mut Context<Self>) -> Div {
        let mut hosts: Vec<_> = self.hosts.as_ref().into_iter().flatten().cloned().collect();
        let mut extra: Vec<_> = self
            .known_sessions
            .keys()
            .chain(self.destination.iter())
            .filter_map(|destination| match destination {
                Destination::Ssh(alias) => Some(alias),
                _ => None,
            })
            .filter(|alias| !hosts.contains(alias))
            .cloned()
            .collect();
        extra.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        extra.dedup();
        hosts.extend(extra);
        let note = if self.host_scan.is_some() {
            Some("Reading SSH configuration…")
        } else {
            match &self.hosts {
                Err(_) => Some("Could not read the SSH configuration. Enter an alias below."),
                Ok(_) if hosts.is_empty() => Some("No SSH hosts found. Enter an alias below."),
                Ok(_) => None,
            }
        };
        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(5.))
            .w_full()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(14.))
                            .text_color(rgb(TEXT))
                            .child("SSH Hosts"),
                    )
                    .child(
                        button("refresh-ssh-hosts", "Refresh", false)
                            .on_click(cx.listener(|view, _, _, cx| view.refresh_hosts(cx))),
                    ),
            )
            .when_some(note, |list, message| {
                list.child(
                    div()
                        .p(px(10.))
                        .rounded_md()
                        .bg(rgb(PANEL))
                        .text_size(px(13.))
                        .text_color(rgb(MUTED))
                        .child(message),
                )
            })
            .children(
                hosts.into_iter().map(|alias| {
                    self.connection_branch(&Destination::Ssh(alias), ui, changing, cx)
                }),
            )
    }

    #[cfg(windows)]
    fn wsl_list(&self, ui: Option<&Ui>, changing: bool, cx: &mut Context<Self>) -> Div {
        let mut distributions: Vec<_> = self
            .distributions
            .as_ref()
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        for destination in self.known_sessions.keys().chain(self.destination.iter()) {
            if let Destination::Wsl(distribution) = destination
                && !distributions.contains(distribution)
            {
                distributions.push(distribution.clone());
            }
        }
        distributions.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        let note = if self.wsl_scan.is_some() {
            Some("Reading WSL distributions…".to_owned())
        } else {
            match &self.distributions {
                Err(error) => Some(error.to_string()),
                Ok(_) if distributions.is_empty() => {
                    Some("No WSL distributions installed.".to_owned())
                }
                Ok(_) => None,
            }
        };
        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(5.))
            .w_full()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(14.)).text_color(rgb(TEXT)).child("WSL"))
                    .child(
                        button("refresh-wsl", "Refresh", false)
                            .on_click(cx.listener(|view, _, _, cx| view.refresh_wsl(cx))),
                    ),
            )
            .when_some(note, |list, message| {
                list.child(
                    div()
                        .p(px(10.))
                        .text_size(px(13.))
                        .text_color(rgb(MUTED))
                        .child(message),
                )
            })
            .children(distributions.into_iter().map(|distribution| {
                self.connection_branch(&Destination::Wsl(distribution), ui, changing, cx)
            }))
    }

    fn connection_branch(
        &self,
        destination: &Destination,
        ui: Option<&Ui>,
        changing: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let (id, label) = match destination {
            Destination::Local => ("local-terminal".to_owned(), "Local Terminal".to_owned()),
            Destination::Ssh(alias) => {
                (format!("ssh-{}", alias.as_str()), alias.as_str().to_owned())
            }
            #[cfg(windows)]
            Destination::Wsl(distribution) => (
                format!("wsl-{}", distribution.as_str()),
                distribution.as_str().to_owned(),
            ),
        };
        let current = ui.filter(|_| self.destination.as_ref() == Some(destination));
        let connected = current.is_some_and(|ui| ui.connected);
        let known = self.known_sessions.get(destination);
        let status = if connected {
            "Connected"
        } else if current.is_some_and(|ui| ui.changing) {
            "Connecting…"
        } else if known.is_some() {
            "Reconnect ›"
        } else {
            "Connect ›"
        };
        let connection = destination.clone();
        div()
            .id(SharedString::from(id))
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(4.))
            .child(
                div()
                    .id("open-connection")
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .py(px(8.))
                    .rounded_md()
                    .bg(rgb(if connected { ACTIVE_FILL } else { BACKGROUND }))
                    .text_color(rgb(TEXT))
                    .text_size(px(13.))
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(12.))
                            .text_color(rgb(if connected { ACTIVE } else { MUTED }))
                            .child(status),
                    )
                    .when(!changing, |row| {
                        row.cursor_pointer()
                            .hover(|style| style.bg(rgb(ACTIVE_FILL)))
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.switch_to(
                                    connection.clone(),
                                    AttachTarget::Default,
                                    window,
                                    cx,
                                )
                            }))
                    }),
            )
            .when_some(known, |branch, known| {
                branch.child(self.session_rows(destination, known, current, changing, cx))
            })
    }

    fn session_rows(
        &self,
        destination: &Destination,
        known: &KnownSessions,
        ui: Option<&Ui>,
        changing: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let workspaces = &known.inventory.workspaces;
        let connected = ui.is_some_and(|ui| ui.connected);
        let selected = ui
            .filter(|ui| ui.connected)
            .and_then(|ui| ui.selected.as_ref());
        div()
            .flex()
            .flex_col()
            .ml(px(14.))
            .pl(px(7.))
            .border_l_1()
            .border_color(rgb(BORDER))
            .gap(px(3.))
            .children(workspaces.iter().map(|workspace| {
                let id = workspace.workspace_id.clone();
                let active = selected.is_some_and(|view| view.workspace_id == id);
                let destination = destination.clone();
                div()
                    .id(SharedString::from(id.clone()))
                    .flex()
                    .flex_none()
                    .items_center()
                    .px(px(9.))
                    .py(px(5.))
                    .rounded_md()
                    .bg(rgb(if active { ACTIVE_FILL } else { PANEL }))
                    .text_size(px(13.))
                    .text_color(rgb(if active { ACTIVE } else { MUTED }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(workspace.label.clone()),
                    )
                    .when(
                        ui.is_some_and(|ui| ui.connected && ui.close_supported),
                        |row| {
                            row.child(Self::close_button(
                                "close-session",
                                "Close this session?",
                                CloseView {
                                    workspace_id: workspace.workspace_id.clone(),
                                    ..Default::default()
                                },
                                cx,
                            ))
                        },
                    )
                    .when(
                        !changing
                            && (!connected
                                || ui.is_some_and(|ui| ui.inventory_ready && !ui.changing)),
                        |row| {
                            row.cursor_pointer()
                                .hover(|style| style.text_color(rgb(TEXT)))
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.open_workspace(
                                        destination.clone(),
                                        id.clone(),
                                        window,
                                        cx,
                                    );
                                }))
                        },
                    )
            }))
            .when(
                connected && !changing && ui.is_some_and(|ui| ui.inventory_ready && !ui.changing),
                |list| {
                    let count = workspaces.len();
                    list.child(
                        button("new-session", "+ New session", false)
                            .on_click(cx.listener(move |view, _, _, _| view.new_workspace(count))),
                    )
                },
            )
    }

    fn tabs(
        &self,
        workspace: &WorkspaceState,
        inventory: &Inventory,
        ui: &Ui,
        cx: &mut Context<Self>,
    ) -> Div {
        let selected = ui.selected.as_ref().map(|view| view.tab_id.as_str());
        let count = workspace.tabs.len();
        div()
            .flex()
            .items_center()
            .gap(px(7.))
            .px(px(12.))
            .py(px(7.))
            .bg(rgb(PANEL))
            .child(
                div()
                    .text_color(rgb(MUTED))
                    .text_size(px(12.))
                    .child("TABS"),
            )
            .child(
                div()
                    .id("daemon-tabs")
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(5.))
                    .overflow_x_scroll()
                    .children(workspace.tabs.iter().filter_map(|tab| {
                        let (target, open) = self.choose(workspace, tab, inventory)?;
                        Some(
                            div()
                                .id(SharedString::from(tab.tab_id.clone()))
                                .flex()
                                .flex_none()
                                .items_center()
                                .rounded_md()
                                .bg(rgb(if selected == Some(&tab.tab_id) {
                                    ACTIVE_FILL
                                } else {
                                    PANEL
                                }))
                                .child(
                                    button(
                                        "select-tab",
                                        tab.label.clone(),
                                        selected == Some(&tab.tab_id),
                                    )
                                    .on_click(cx.listener(
                                        move |view, _, window, cx| {
                                            view.select(target.clone(), open, window, cx)
                                        },
                                    )),
                                )
                                .when(ui.connected && ui.close_supported, |row| {
                                    row.child(Self::close_button(
                                        "close-tab",
                                        "Close this tab?",
                                        CloseView {
                                            workspace_id: workspace.workspace_id.clone(),
                                            tab_id: Some(tab.tab_id.clone()),
                                            ..Default::default()
                                        },
                                        cx,
                                    ))
                                }),
                        )
                    })),
            )
            .when(ui.connected, |bar| {
                bar.child(button("new-tab", "+ New tab", false).on_click(cx.listener(
                    move |view, _, _, _| {
                        view.create(Target::TabLabel(format!("Shell {}", count + 1)))
                    },
                )))
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
                .gap(px(5.))
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
                let title = format!("Pane {}", *ordinal);
                let preview_session = self
                    .session
                    .clone()
                    .filter(|_| ui.connected && live && !active);
                region
                    .flex_col()
                    .rounded_md()
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
                            .child(div().flex_1().child(title))
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
                                .p(px(10.))
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
            .when_some(workspace, |region, workspace| {
                region.child(self.tabs(workspace, inventory, ui, cx))
            })
            .child(div().flex().flex_1().min_h_0().p(px(5.)).child(body))
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = self.session.as_ref().map(Session::ui);
        let connected = ui.as_ref().is_some_and(|ui| ui.connected);
        let inventory_ready = ui.as_ref().is_some_and(|ui| ui.inventory_ready);
        let inventory: Option<&Arc<Inventory>> = ui.as_ref().and_then(|ui| ui.inventory.as_ref());
        let workspace = ui
            .as_ref()
            .and_then(|ui| ui.selected.as_ref())
            .and_then(|view| {
                inventory?
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == view.workspace_id)
            });
        let content = if let Some((ui, inventory)) = ui.as_ref().zip(inventory) {
            self.terminal_layout(ui, inventory, cx).into_any_element()
        } else if connected {
            div()
                .flex_1()
                .min_h_0()
                .children(self.terminal.iter().cloned())
                .into_any_element()
        } else {
            self.terminal_placeholder(ui.as_ref(), cx)
                .into_any_element()
        };
        div()
            .id("corvid-workspace")
            .track_focus(&self.setup_focus)
            .capture_key_down(cx.listener(Self::key_down))
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BACKGROUND))
            .child(self.toolbar(
                connected,
                workspace.map(|workspace| workspace.label.as_str()),
                cx,
            ))
            .when(self.plugins_open, |region| {
                region.child(self.plugins_menu(connected, cx))
            })
            .when(self.help_open, |region| region.child(self.update_menu(cx)))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .when(self.connections_open, |body| {
                        body.child(self.connections_panel(ui.as_ref(), cx))
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .child(content),
                    ),
            )
            .when_some(
                ui.filter(|ui| ui.connected || ui.inventory.is_some()),
                |region, ui| {
                    region.child(
                        div()
                            .flex_none()
                            .px(px(12.))
                            .py(px(5.))
                            .bg(rgb(PANEL))
                            .text_color(rgb(MUTED))
                            .text_size(px(12.))
                            .truncate()
                            .child(if self.host_switch.is_some() {
                                "Switching connection…".into()
                            } else if connected && !inventory_ready {
                                format!("{} · Syncing daemon inventory", ui.message)
                            } else {
                                ui.message
                            }),
                    )
                },
            )
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
        .cursor_pointer()
        .hover(|style| style.text_color(rgb(TEXT)))
        .child(label.into())
}

fn alias_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"._-".contains(&byte)
}
