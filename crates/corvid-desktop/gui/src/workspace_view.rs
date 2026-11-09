use std::collections::HashMap;
use std::path::PathBuf;

use corvid_core::layout::{Axis, Node};
use corvid_deck::deck::setup::{DesktopLifetime, SetupView};
use gpui::{
    AppContext, Context, Div, Entity, IntoElement, KeyDownEvent, SharedString, Stateful, Window,
    div, prelude::*, px, rgb,
};
use uuid::Uuid;

use crate::session_layout::SessionLayout;
use crate::terminal_view::{Lifetime, PaneFocused, TerminalView};

const BACKGROUND: u32 = 0x101318;
const PANEL: u32 = 0x191c23;
const BORDER: u32 = 0x30343f;
const MUTED: u32 = 0xa4adbc;
const ACTIVE: u32 = 0x83dec0;
const ACTIVE_FILL: u32 = 0x20332f;

pub struct AgentSetup {
    pub instance: String,
    pub state: PathBuf,
    pub alias: Option<String>,
}

pub struct WorkspaceView {
    layout: SessionLayout,
    panes: HashMap<Uuid, Entity<TerminalView>>,
    terminal_lifetime: Lifetime,
    agents: Option<Entity<SetupView>>,
    setup: Option<AgentSetup>,
    desktop_lifetime: DesktopLifetime,
    show_agents: bool,
}

impl WorkspaceView {
    pub fn new(
        setup: AgentSetup,
        desktop_lifetime: DesktopLifetime,
        terminal_lifetime: Lifetime,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            layout: SessionLayout::default(),
            panes: HashMap::new(),
            terminal_lifetime,
            agents: None,
            setup: Some(setup),
            desktop_lifetime,
            show_agents: false,
        };
        view.open_shell(view.layout.active().pane, window, cx);
        view
    }

    fn open_shell(&mut self, pane: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let terminal =
            cx.new(|cx| TerminalView::new(pane, self.terminal_lifetime.clone(), window, cx));
        cx.subscribe(&terminal, move |view, _, _: &PaneFocused, cx| {
            if !view.show_agents
                && view.layout.active().pane != pane
                && view.layout.select_pane(pane)
            {
                cx.notify();
            }
        })
        .detach();
        self.panes.insert(pane, terminal);
        self.show_agents = false;
        cx.notify();
    }

    fn select_tab(&mut self, tab: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if self.layout.select_tab(tab) {
            self.show_agents = false;
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn focus_active(&self, window: &mut Window, cx: &Context<Self>) {
        if let Some(terminal) = self.panes.get(&self.layout.active().pane) {
            terminal.read(cx).focus(window);
        }
    }

    fn select_agents(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_agents {
            return;
        }
        self.show_agents = true;
        if let Some(setup) = self.setup.take() {
            self.agents = Some(cx.new(|cx| {
                SetupView::new(
                    setup.instance,
                    setup.state,
                    setup.alias,
                    self.desktop_lifetime.clone(),
                    window,
                    cx,
                )
            }));
        } else if let Some(agents) = &self.agents {
            agents.read(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn add_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal_lifetime.has_capacity()
            && let Some(pane) = self.layout.add_tab()
        {
            self.open_shell(pane, window, cx);
        }
    }

    fn split(&mut self, axis: Axis, window: &mut Window, cx: &mut Context<Self>) {
        if !self.show_agents
            && self.terminal_lifetime.has_capacity()
            && let Some(pane) = self.layout.split(axis)
        {
            self.open_shell(pane, window, cx);
        }
    }

    fn close_pane(&mut self, pane: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if !self.show_agents && self.layout.close_pane(pane) {
            self.close_shells([pane]);
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn close_tab(&mut self, tab: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let panes = self.layout.close_tab(tab);
        if panes.is_empty() {
            return;
        }
        self.close_shells(panes);
        if !self.show_agents {
            self.focus_active(window, cx);
        }
        cx.notify();
    }

    fn close_shells(&mut self, panes: impl IntoIterator<Item = Uuid>) {
        for pane in panes {
            self.terminal_lifetime.close(pane);
            self.panes.remove(&pane);
        }
    }

    fn shortcuts(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        let modifiers = key.modifiers;
        if !modifiers.control || modifiers.alt || modifiers.platform {
            return;
        }
        let name = key.key.to_ascii_lowercase();
        let shortcut = (modifiers.shift, name.as_str());
        if event.is_held && matches!(shortcut, (true, "t" | "d" | "e" | "w") | (_, "tab")) {
            cx.stop_propagation();
            return;
        }
        match shortcut {
            (true, "t") => self.add_tab(window, cx),
            (true, "d") => self.split(Axis::Horizontal, window, cx),
            (true, "e") => self.split(Axis::Vertical, window, cx),
            (true, "w") => self.close_pane(self.layout.active().pane, window, cx),
            (backward, "tab") => {
                self.layout.cycle_tab(!backward);
                self.show_agents = false;
                self.focus_active(window, cx);
                cx.notify();
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn tabs(&self, cx: &mut Context<Self>) -> Div {
        let can_close = self.layout.tabs().len() > 1;
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .py(px(8.))
            .bg(rgb(PANEL))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .px(px(7.))
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child("Local session"),
            )
            .child(
                div()
                    .id("session-tabs")
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(px(5.))
                    .overflow_x_scroll()
                    .children(self.layout.tabs().iter().map(|tab| {
                        let id = tab.id;
                        let selected = !self.show_agents && id == self.layout.active().tab;
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .rounded_md()
                            .bg(rgb(if selected { ACTIVE_FILL } else { BACKGROUND }))
                            .text_color(rgb(if selected { ACTIVE } else { MUTED }))
                            .child(
                                div()
                                    .id(SharedString::from(format!("tab-{id}")))
                                    .cursor_pointer()
                                    .px(px(10.))
                                    .py(px(6.))
                                    .text_size(px(13.))
                                    .child(tab.label.clone())
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        view.select_tab(id, window, cx)
                                    })),
                            )
                            .when(can_close, |item| {
                                item.child(
                                    div()
                                        .id(SharedString::from(format!("close-tab-{id}")))
                                        .px(px(7.))
                                        .cursor_pointer()
                                        .hover(|style| style.text_color(rgb(0xf4a3bb)))
                                        .child("×")
                                        .on_click(cx.listener(move |view, _, window, cx| {
                                            view.close_tab(id, window, cx)
                                        })),
                                )
                            })
                    })),
            )
            .child(
                button("new-local-tab", "+ New tab", self.layout.can_add_tab())
                    .on_click(cx.listener(|view, _, window, cx| view.add_tab(window, cx))),
            )
            .child(
                div()
                    .id("session-agents")
                    .px(px(12.))
                    .py(px(6.))
                    .rounded_md()
                    .bg(rgb(if self.show_agents {
                        ACTIVE_FILL
                    } else {
                        BACKGROUND
                    }))
                    .text_color(rgb(if self.show_agents { ACTIVE } else { MUTED }))
                    .text_size(px(13.))
                    .cursor_pointer()
                    .child("Agents")
                    .on_click(cx.listener(|view, _, window, cx| view.select_agents(window, cx))),
            )
    }

    fn controls(&self, cx: &mut Context<Self>) -> Div {
        let can_split = self.layout.can_split();
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(7.))
            .px(px(12.))
            .py(px(7.))
            .bg(rgb(BACKGROUND))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(button("split-right", "Split right", can_split).on_click(
                cx.listener(|view, _, window, cx| view.split(Axis::Horizontal, window, cx)),
            ))
            .child(button("split-down", "Split down", can_split).on_click(
                cx.listener(|view, _, window, cx| view.split(Axis::Vertical, window, cx)),
            ))
            .child(div().flex_1())
            .child(
                div().text_size(px(11.)).text_color(rgb(MUTED)).child(
                    "Ctrl+Shift+T New tab  ·  D Split right  ·  E Split down  ·  W Close pane",
                ),
            )
    }

    fn pane_tree(&self, node: &Node, order: &mut usize, cx: &mut Context<Self>) -> Div {
        let region = div().flex().flex_1().min_w_0().min_h_0().overflow_hidden();
        match node {
            Node::Split { axis, children } => region
                .when(matches!(axis, Axis::Vertical), |split| split.flex_col())
                .gap(px(5.))
                .children(
                    children
                        .iter()
                        .map(|child| self.pane_tree(child, order, cx)),
                ),
            Node::Pane { id, .. } => {
                *order += 1;
                let pane = *id;
                let focused = self.layout.active().pane == pane;
                region
                    .flex_col()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(if focused { 0x4b8e7b } else { BORDER }))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_between()
                            .px(px(10.))
                            .h(px(27.))
                            .bg(rgb(if focused { ACTIVE_FILL } else { PANEL }))
                            .text_color(rgb(if focused { ACTIVE } else { MUTED }))
                            .text_size(px(12.))
                            .child(format!("Pane {}", *order))
                            .when(self.layout.can_close_pane(), |header| {
                                header.child(
                                    div()
                                        .id(SharedString::from(format!("close-pane-{pane}")))
                                        .px(px(6.))
                                        .cursor_pointer()
                                        .hover(|style| style.text_color(rgb(0xf4a3bb)))
                                        .child("×")
                                        .on_click(cx.listener(move |view, _, window, cx| {
                                            view.close_pane(pane, window, cx)
                                        })),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .children(self.panes.get(&pane).cloned()),
                    )
            }
        }
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = if self.show_agents {
            div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .children(self.agents.iter().cloned())
        } else {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(self.controls(cx))
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_h_0()
                        .p(px(5.))
                        .child(self.pane_tree(&self.layout.active_tab().root, &mut 0, cx)),
                )
        };
        div()
            .id("session-workspace")
            .capture_key_down(cx.listener(Self::shortcuts))
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BACKGROUND))
            .child(self.tabs(cx))
            .child(content)
    }
}

fn button(id: &'static str, label: &'static str, enabled: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .px(px(10.))
        .py(px(5.))
        .rounded_md()
        .bg(rgb(PANEL))
        .text_size(px(12.))
        .text_color(rgb(if enabled { MUTED } else { 0x697281 }))
        .when(enabled, |button| {
            button
                .cursor_pointer()
                .hover(|style| style.text_color(rgb(ACTIVE)))
        })
        .child(label)
}
