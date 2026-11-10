use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use corvid_client::SshAlias;
use corvid_deck::icons::icon;
use corvid_protocol::wire::create_view::Target;
use corvid_protocol::wire::layout_node_state::Body as Node;
use corvid_protocol::wire::{
    AgentBadge, AgentInfo, AgentProvider, SplitAxis, TabState, ViewSelection, WorkspaceState,
};
use gpui::{
    AppContext, Context, Div, Entity, FocusHandle, IntoElement, KeyDownEvent, SharedString,
    Stateful, Task, Window, div, prelude::*, px, rgb,
};

use crate::terminal_session::{Inventory, Job, Session, Ui};
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
    configured: Option<(String, String)>,
    setup_focus: FocusHandle,
    session: Option<Session>,
    terminal: Option<Entity<TerminalView>>,
    selected: Option<ViewSelection>,
    preferred_tabs: HashMap<String, String>,
    preferred_panes: HashMap<String, String>,
    updates: Option<Task<()>>,
    error: Option<String>,
    show_deck: bool,
}

impl WorkspaceView {
    pub fn new(
        alias: Option<String>,
        configured: Option<(String, String)>,
        lifetime: Lifetime,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let alias = alias
            .or_else(|| configured.as_ref().map(|(alias, _)| alias.clone()))
            .unwrap_or_default();
        let setup_focus = cx.focus_handle();
        window.focus(&setup_focus);
        let mut view = Self {
            lifetime,
            alias,
            configured,
            setup_focus,
            session: None,
            terminal: None,
            selected: None,
            preferred_tabs: HashMap::new(),
            preferred_panes: HashMap::new(),
            updates: None,
            error: None,
            show_deck: false,
        };
        if !view.alias.is_empty() {
            view.connect(None, window, cx);
        }
        view
    }

    fn connect(
        &mut self,
        selection: Option<(ViewSelection, bool)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.lifetime.available() {
            return;
        }
        let expected_host = self
            .configured
            .as_ref()
            .filter(|(alias, _)| alias == &self.alias)
            .map(|(_, host)| host.clone());
        let result = SshAlias::new(self.alias.clone())
            .and_then(|alias| Session::start(alias, expected_host, selection));
        let (session, job) = match result {
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
        self.show_deck = false;
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
                if changes.changed().await.is_err() {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        let ui = session.ui();
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
            if !self.show_deck {
                self.focus_terminal(window, cx);
            }
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
        run: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = &self.session {
            if session.ui().connected {
                session.select(target, open, run);
                self.show_deck = false;
                self.focus_terminal(window, cx);
                cx.notify();
                return;
            }
        }
        if run.is_none() {
            self.connect(Some((target, open)), window, cx);
        }
    }

    fn create(&self, target: Target) {
        if let Some(session) = &self.session {
            session.create(target);
        }
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
        if !ui.as_ref().is_some_and(|ui| ui.connected) {
            if ui.as_ref().is_some_and(|ui| ui.changing) {
                return;
            }
            if key.key == "enter" {
                self.connect(None, window, cx);
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
                    self.select(target, open, None, window, cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn toolbar(&self, connected: bool, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .p(px(10.))
            .bg(rgb(PANEL))
            .child(div().flex_1().text_color(rgb(TEXT)).child("Corvid Desktop"))
            .child(
                button("terminal-mode", "Terminal", !self.show_deck).on_click(cx.listener(
                    |view, _, window, cx| {
                        view.show_deck = false;
                        view.focus_terminal(window, cx);
                        cx.notify();
                    },
                )),
            )
            .child(
                button("agent-deck", "Agent Deck", self.show_deck).on_click(cx.listener(
                    |view, _, window, cx| {
                        view.show_deck = true;
                        window.focus(&view.setup_focus);
                        cx.notify();
                    },
                )),
            )
            .when(
                !connected
                    && self
                        .session
                        .as_ref()
                        .is_some_and(|session| !session.ui().changing),
                |bar| {
                    bar.child(button("connection-settings", "Connect…", false).on_click(
                        cx.listener(|view, _, window, cx| {
                            view.session = None;
                            view.terminal = None;
                            view.updates = None;
                            view.show_deck = false;
                            window.focus(&view.setup_focus);
                            cx.notify();
                        }),
                    ))
                },
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
    }

    fn setup(&self, ui: Option<&Ui>, cx: &mut Context<Self>) -> Div {
        let changing = ui.is_some_and(|ui| ui.changing);
        let message = self
            .error
            .as_deref()
            .or_else(|| ui.map(|ui| ui.message.as_str()))
            .unwrap_or("Connect to a running Corvid daemon on a Linux host.");
        div()
            .flex()
            .flex_col()
            .flex_1()
            .items_center()
            .justify_center()
            .gap(px(14.))
            .child(
                div()
                    .text_color(rgb(TEXT))
                    .text_size(px(22.))
                    .child("Corvid daemon"),
            )
            .child(
                div()
                    .max_w(px(650.))
                    .text_center()
                    .text_color(rgb(MUTED))
                    .child(message.to_owned()),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .items_center()
                    .child(
                        div()
                            .id("ssh-alias")
                            .w(px(300.))
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
                                cx.listener(|view, _, window, _| window.focus(&view.setup_focus)),
                            ),
                    )
                    .child(
                        button(
                            "connect",
                            if changing { "Connecting…" } else { "Connect" },
                            true,
                        )
                        .on_click(
                            cx.listener(|view, _, window, cx| view.connect(None, window, cx)),
                        ),
                    ),
            )
    }

    fn sessions(&self, inventory: &Inventory, ui: &Ui, cx: &mut Context<Self>) -> Div {
        let count = inventory.workspaces.len();
        let selected = ui.selected.as_ref().map(|view| view.workspace_id.as_str());
        div()
            .flex()
            .items_center()
            .gap(px(7.))
            .px(px(12.))
            .py(px(7.))
            .child(
                div()
                    .text_color(rgb(MUTED))
                    .text_size(px(12.))
                    .child("SESSIONS"),
            )
            .child(
                div()
                    .id("daemon-sessions")
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(5.))
                    .overflow_x_scroll()
                    .children(inventory.workspaces.iter().filter_map(|workspace| {
                        let tab = workspace
                            .tabs
                            .iter()
                            .find(|tab| {
                                self.preferred_tabs.get(&workspace.workspace_id)
                                    == Some(&tab.tab_id)
                            })
                            .or_else(|| workspace.tabs.first())?;
                        let (target, open) = self.choose(workspace, tab, inventory)?;
                        Some(
                            button(
                                SharedString::from(workspace.workspace_id.clone()),
                                workspace.label.clone(),
                                selected == Some(&workspace.workspace_id),
                            )
                            .on_click(cx.listener(
                                move |view, _, window, cx| {
                                    view.select(target.clone(), open, None, window, cx)
                                },
                            )),
                        )
                    })),
            )
            .when(ui.connected, |bar| {
                bar.child(
                    button("new-session", "+ New session", false)
                        .on_click(cx.listener(move |view, _, _, _| view.new_workspace(count))),
                )
            })
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
                            button(
                                SharedString::from(tab.tab_id.clone()),
                                tab.label.clone(),
                                selected == Some(&tab.tab_id),
                            )
                            .on_click(cx.listener(
                                move |view, _, window, cx| {
                                    view.select(target.clone(), open, None, window, cx)
                                },
                            )),
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

    fn controls(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(7.))
            .px(px(12.))
            .py(px(7.))
            .child(
                button("split-right", "Split right", false).on_click(cx.listener(
                    |view, _, _, _| view.create(Target::SplitAxis(SplitAxis::Horizontal as i32)),
                )),
            )
            .child(
                button("split-down", "Split down", false).on_click(cx.listener(|view, _, _, _| {
                    view.create(Target::SplitAxis(SplitAxis::Vertical as i32))
                })),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_color(rgb(MUTED))
                    .text_size(px(11.))
                    .child("Ctrl+Shift+N Session  ·  T Tab  ·  D Split right  ·  E Split down"),
            )
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
                            .px(px(9.))
                            .py(px(4.))
                            .bg(rgb(if active { ACTIVE_FILL } else { PANEL }))
                            .text_color(rgb(if active { ACTIVE } else { MUTED }))
                            .text_size(px(12.))
                            .child(title),
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
                                    view.select(target.clone(), false, None, window, cx)
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
                                        view.select(target.clone(), !live, None, window, cx)
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
            return self.setup(Some(ui), cx);
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
            .child(self.sessions(inventory, ui, cx))
            .when_some(workspace, |region, workspace| {
                region.child(self.tabs(workspace, inventory, ui, cx))
            })
            .when(ui.connected, |region| region.child(self.controls(cx)))
            .child(div().flex().flex_1().min_h_0().p(px(5.)).child(body))
    }

    fn agent_deck(
        &self,
        inventory: Option<&Inventory>,
        connected: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let mut agents: Vec<_> = inventory
            .into_iter()
            .flat_map(|inventory| &inventory.agents)
            .collect();
        agents.sort_by_key(|agent| {
            (
                agent.badge == AgentBadge::Ended as i32,
                agent.agent_run_id.as_str(),
            )
        });
        let mut slots: Vec<_> = agents
            .into_iter()
            .map(|agent| self.agent_key(agent, connected, cx))
            .collect();
        let capacity = slots.len().max(1).div_ceil(5) * 5;
        slots.extend((slots.len()..capacity).map(|index| {
            key_tile(SharedString::from(format!("empty-{index}")), BORDER, PANEL)
                .child(icon("grid", 22., MUTED))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(MUTED))
                        .child("Empty"),
                )
        }));
        let mut slots = slots.into_iter();
        div()
            .id("agent-grid")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(px(18.))
            .gap(px(12.))
            .child(
                div()
                    .text_size(px(21.))
                    .text_color(rgb(TEXT))
                    .child("Agent Deck"),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(rgb(MUTED))
                    .child(if connected {
                        "Daemon-observed agent activity. Select a key to focus its existing pane."
                    } else {
                        "Offline. Connect a Corvid daemon to see live agent activity."
                    }),
            )
            .children((0..capacity / 5).map(|_| {
                div()
                    .flex()
                    .flex_none()
                    .gap(px(9.))
                    .children(slots.by_ref().take(5))
            }))
    }

    fn agent_key(
        &self,
        agent: &AgentInfo,
        connected: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let (name, glyph) = match AgentProvider::try_from(agent.provider) {
            Ok(AgentProvider::Claude) => ("Claude Code", "brand"),
            Ok(AgentProvider::Codex) => ("Codex", "focus"),
            _ => ("Agent", "grid"),
        };
        let (status, ink, fill) = badge(agent.badge);
        let run = agent.agent_run_id.clone();
        let target = agent
            .location
            .clone()
            .filter(|_| connected && agent.terminal_available);
        key_tile(SharedString::from(run.clone()), ink, fill)
            .child(icon(glyph, 26., ink))
            .child(div().text_color(rgb(TEXT)).child(name))
            .child(div().text_color(rgb(ink)).text_size(px(12.)).child(status))
            .when_some(target, |key, target| {
                key.cursor_pointer()
                    .hover(move |style| style.border_color(rgb(TEXT)))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.select(target.clone(), false, Some(run.clone()), window, cx)
                    }))
            })
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = self.session.as_ref().map(Session::ui);
        let connected = ui.as_ref().is_some_and(|ui| ui.connected);
        let inventory_ready = ui.as_ref().is_some_and(|ui| ui.inventory_ready);
        let inventory: Option<&Arc<Inventory>> = ui.as_ref().and_then(|ui| ui.inventory.as_ref());
        let content = if self.show_deck {
            self.agent_deck(inventory.map(AsRef::as_ref), inventory_ready, cx)
                .into_any_element()
        } else if let Some((ui, inventory)) = ui.as_ref().zip(inventory) {
            self.terminal_layout(ui, inventory, cx).into_any_element()
        } else if connected {
            div()
                .flex_1()
                .min_h_0()
                .children(self.terminal.iter().cloned())
                .into_any_element()
        } else {
            self.setup(ui.as_ref(), cx).into_any_element()
        };
        div()
            .id("corvid-workspace")
            .track_focus(&self.setup_focus)
            .capture_key_down(cx.listener(Self::key_down))
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BACKGROUND))
            .child(self.toolbar(connected, cx))
            .child(content)
            .when_some(ui, |region, ui| {
                region.child(
                    div()
                        .flex_none()
                        .px(px(12.))
                        .py(px(5.))
                        .bg(rgb(PANEL))
                        .text_color(rgb(MUTED))
                        .text_size(px(12.))
                        .truncate()
                        .child(if connected && !inventory_ready {
                            format!("{} · Syncing daemon inventory", ui.message)
                        } else {
                            ui.message
                        }),
                )
            })
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

fn key_tile(id: SharedString, ink: u32, fill: u32) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .flex_none()
        .size(px(133.))
        .items_center()
        .justify_center()
        .gap(px(5.))
        .rounded_lg()
        .border_1()
        .border_color(rgb(ink))
        .bg(rgb(fill))
        .shadow_md()
}

fn badge(value: i32) -> (&'static str, u32, u32) {
    match AgentBadge::try_from(value) {
        Ok(AgentBadge::Working) => ("Working", 0xf3cc82, 0x392d21),
        Ok(AgentBadge::NeedsInput) => ("Input needed", 0xf3cc82, 0x392d21),
        Ok(AgentBadge::ResponseObserved) => ("Response observed", ACTIVE, ACTIVE_FILL),
        Ok(AgentBadge::Ready) => ("Ready", 0x9bc5f8, 0x213047),
        Ok(AgentBadge::Ended) => ("Ended", MUTED, PANEL),
        Ok(AgentBadge::Interrupted) => ("Interrupted", 0xf3cc82, 0x392d21),
        Ok(AgentBadge::Error) => ("Error", 0xf4a3bb, 0x3b2530),
        _ => ("Unknown", MUTED, PANEL),
    }
}
