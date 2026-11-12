use super::*;
use gpui::{Bounds, TitlebarOptions, WeakEntity, WindowBounds, WindowOptions, size};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    Connections,
    Terminal,
    Shortcuts,
    Extensions,
    About,
}

pub(super) struct ManagementView {
    owner: WeakEntity<WorkspaceView>,
}

impl ManagementView {
    fn new(owner: Entity<WorkspaceView>, window: &Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&owner, |_, _, cx| cx.notify()).detach();
        cx.observe_release_in(&owner, window, |_, _, window, _| window.remove_window())
            .detach();
        Self {
            owner: owner.downgrade(),
        }
    }
}

impl Render for ManagementView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.owner
            .update(cx, |owner, cx| owner.management_ui(cx))
            .unwrap_or_else(|_| div().id("settings-unavailable"))
    }
}

impl WorkspaceView {
    pub(super) fn open_management(
        &mut self,
        page: Page,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.management_page = page;
        self.dismiss_overlay(window, cx);
        self.refresh_hosts(cx);
        #[cfg(windows)]
        self.refresh_wsl(cx);
        let owner = cx.entity();
        let focus = self.management_focus.clone();
        // GPUI draws a new window before open_window returns. Defer creation so
        // its first render can read the owner after this update has finished.
        cx.defer(move |cx| {
            if owner.read(cx).management_window.is_some_and(|handle| {
                handle
                    .update(cx, |_, window, _| window.activate_window())
                    .is_ok()
            }) {
                return;
            }
            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Corvid Settings".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(740.), px(580.)),
                    cx,
                ))),
                window_min_size: Some(size(px(580.), px(420.))),
                ..Default::default()
            };
            let settings_owner = owner.clone();
            let result = cx.open_window(options, move |window, cx| {
                window.focus(&focus);
                cx.new(|cx| ManagementView::new(settings_owner, window, cx))
            });
            owner.update(cx, |view, cx| {
                match result {
                    Ok(handle) => view.management_window = Some(handle),
                    Err(error) => view.error = Some(format!("Could not open settings: {error}")),
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    fn management_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = &event.keystroke;
        if key.key == "escape" {
            window.remove_window();
            let terminal = self.terminal_window;
            cx.defer(move |cx| {
                let _ = terminal.update(cx, |view, window, cx| {
                    window.activate_window();
                    view.focus_terminal(window, cx);
                });
            });
            cx.stop_propagation();
            return;
        }
        if !self.alias_focus.is_focused(window) || self.management_page != Page::Connections {
            return;
        }
        cx.stop_propagation();
        if self.connecting(self.session.as_ref().map(Session::ui).as_ref()) {
            return;
        }
        if key.key == "enter" {
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
        }
        cx.notify();
    }

    fn management_ui(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let ui = self.session.as_ref().map(Session::ui);
        let pages = [
            (Page::Connections, "Connections"),
            (Page::Terminal, "Terminal"),
            (Page::Shortcuts, "Shortcuts"),
            (Page::Extensions, "Extensions"),
            (Page::About, "About & updates"),
        ];
        let content = match self.management_page {
            Page::Connections => self.connections_panel(ui.as_ref(), cx),
            Page::Terminal => self.terminal_settings(cx),
            Page::Shortcuts => shortcuts(),
            Page::Extensions => settings_page("Extensions")
                .child(description("Corvid Agent Deck runs in its own window. Questions and approvals stay in the terminal."))
                .when(ui.as_ref().is_some_and(|ui| ui.connected), |page| {
                    page.child(button("open-deck-settings", "Open Agent Deck", true).on_click(
                        cx.listener(|view, _, _, _| {
                            if let Some(session) = &view.session {
                                session.open_agent_deck(view.agent_deck.clone());
                            }
                        }),
                    ))
                })
                .when(!ui.as_ref().is_some_and(|ui| ui.connected), |page| {
                    page.child(description("Connect to a terminal to open its Agent Deck."))
                }),
            Page::About => settings_page("About & updates")
                .child(self.update_menu(cx))
                .child(description("Desktop updates and daemon updates are separate. Connection setup checks the selected daemon when needed.")),
        };
        div()
            .id("management")
            .track_focus(&self.management_focus)
            .capture_key_down(cx.listener(Self::management_key_down))
            .flex()
            .size_full()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .w(px(164.))
                    .p(px(12.))
                    .gap(px(6.))
                    .bg(rgb(PANEL))
                    .children(pages.into_iter().map(|(page, label)| {
                        button(label, label, self.management_page == page).on_click(cx.listener(
                            move |view, _, window, cx| {
                                view.management_page = page;
                                window.focus(&view.management_focus);
                                cx.notify();
                            },
                        ))
                    })),
            )
            .child(
                div()
                    .id("management-body")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(content),
            )
    }

    fn terminal_settings(&self, cx: &mut Context<Self>) -> Div {
        let focus_label = if self.focus_mode {
            "☑ Hide the tab bar (focus mode)"
        } else {
            "☐ Hide the tab bar (focus mode)"
        };
        let focus = button("focus-mode-setting", focus_label, self.focus_mode).on_click(
            cx.listener(|view, _, _, cx| {
                view.focus_mode = !view.focus_mode;
                cx.notify();
            }),
        );
        let zoom_label = if self.zoom_pane {
            "☑ Expand the active pane"
        } else {
            "☐ Expand the active pane"
        };
        let zoom = button("zoom-pane-setting", zoom_label, self.zoom_pane).on_click(cx.listener(
            |view, _, _, cx| {
                view.zoom_pane = !view.zoom_pane;
                cx.notify();
            },
        ));
        let handoff_label = if self.observe_accept_writer {
            "☑ Accept writer handoff when observing"
        } else {
            "☐ Accept writer handoff when observing"
        };
        let handoff = button("accept-writer", handoff_label, self.observe_accept_writer).on_click(
            cx.listener(|view, _, _, cx| {
                view.observe_accept_writer = !view.observe_accept_writer;
                cx.notify();
            }),
        );
        settings_page("Terminal")
            .child(focus)
            .child(zoom)
            .child(description(concat!(
                "Ctrl+Shift+F toggles focus mode. Ctrl+Shift+Z expands the active pane ",
                "and restores the split layout. These view settings apply to this window.",
            )))
            .child(handoff)
            .child(description(concat!(
                "The handoff preference applies the next time you open a read-only view. ",
                "Taking input control always requires an explicit action.",
            )))
    }
}

fn settings_page(title: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .gap(px(16.))
        .p(px(24.))
        .child(div().text_size(px(20.)).child(title))
}

fn description(text: &'static str) -> Div {
    div().text_size(px(13.)).text_color(rgb(MUTED)).child(text)
}

fn shortcuts() -> Div {
    let keys = [
        ("Ctrl+Shift+P", "Switch workspace / search"),
        ("Ctrl+,", "Settings"),
        ("Ctrl+Shift+N", "New workspace"),
        ("Ctrl+Shift+T", "New tab"),
        ("Ctrl+Tab / Ctrl+Shift+Tab", "Next / previous tab"),
        ("Ctrl+Shift+D / Ctrl+Shift+E", "Split right / down"),
        ("Ctrl+Shift+F", "Toggle focus mode"),
        ("Ctrl+Shift+Z", "Expand / restore active pane"),
        ("Ctrl+Shift+R", "Rename pane"),
        ("Ctrl+Alt+R", "Rename tab"),
        ("Ctrl+Shift+C / Ctrl+Shift+V", "Copy / paste"),
        ("Esc", "Close a menu or settings"),
    ];
    settings_page("Shortcuts").children(keys.into_iter().map(|(key, label)| {
        div()
            .flex()
            .justify_between()
            .gap(px(12.))
            .text_size(px(13.))
            .child(label)
            .child(div().text_color(rgb(MUTED)).child(key))
    }))
}
