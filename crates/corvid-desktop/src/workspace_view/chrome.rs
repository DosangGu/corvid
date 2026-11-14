use super::*;
use corvid_protocol::wire::AgentBadge;
use gpui::MouseButton;

#[derive(Clone, Copy)]
pub(super) enum MenuAction {
    Rename,
    NewTab,
    NewWorkspace,
    SplitRight,
    SplitDown,
    Focus,
    Zoom,
    Settings,
    DisconnectOthers,
    Detach,
    ClosePane,
    CloseTab,
    CloseWorkspace,
}

impl WorkspaceView {
    pub(super) fn toggle_menu(
        &mut self,
        create: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.menu_open && self.menu_create == create {
            self.dismiss_overlay(window, cx);
            return;
        }
        self.switcher_open = false;
        self.menu_open = true;
        self.menu_create = create;
        self.menu_index = 0;
        window.focus(&self.setup_focus);
        cx.notify();
    }

    fn menu_items(&self) -> Vec<(MenuAction, &'static str, &'static str)> {
        let ui = self.session.as_ref().map(Session::ui);
        if self.menu_create {
            return if ui.as_ref().is_some_and(|ui| ui.can_write() && !ui.changing) {
                vec![
                    (MenuAction::NewTab, "New tab", "Ctrl+Shift+T"),
                    (MenuAction::NewWorkspace, "New workspace", "Ctrl+Shift+N"),
                    (MenuAction::SplitRight, "Split right", "Ctrl+Shift+D"),
                    (MenuAction::SplitDown, "Split down", "Ctrl+Shift+E"),
                ]
            } else {
                Vec::new()
            };
        }
        let mut items = vec![
            (
                MenuAction::Focus,
                if self.focus_mode {
                    "Show tab bar"
                } else {
                    "Focus mode"
                },
                "Ctrl+Shift+F",
            ),
            (
                MenuAction::Zoom,
                if self.zoom_pane {
                    "Restore split layout"
                } else {
                    "Expand active pane"
                },
                "Ctrl+Shift+Z",
            ),
            (MenuAction::Settings, "Settings…", "Ctrl+,"),
        ];
        if let Some(ui) = ui.filter(|ui| ui.connected && !ui.changing) {
            if ui.can_write() && ui.naming_supported && ui.inventory_ready {
                items.push((MenuAction::Rename, "Rename tab…", "Ctrl+Shift+R"));
            }
            if ui.can_write() && ui.session_clients_supported {
                items.push((
                    MenuAction::DisconnectOthers,
                    "Disconnect other clients…",
                    "",
                ));
            }
            items.push((MenuAction::Detach, "Detach · keep terminals running", ""));
            if ui.can_close() {
                items.extend([
                    (MenuAction::ClosePane, "End this terminal…", ""),
                    (MenuAction::CloseTab, "End this tab…", ""),
                    (MenuAction::CloseWorkspace, "End this workspace…", ""),
                ]);
            }
        }
        items
    }

    pub(super) fn menu_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let items = self.menu_items();
        match event.keystroke.key.as_str() {
            "escape" => self.dismiss_overlay(window, cx),
            "up" => self.menu_index = self.menu_index.saturating_sub(1),
            "down" => self.menu_index = (self.menu_index + 1).min(items.len().saturating_sub(1)),
            "enter" => {
                if let Some((action, _, _)) = items.get(self.menu_index) {
                    self.run_menu_action(*action, window, cx);
                }
            }
            _ => {}
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn run_menu_action(&mut self, action: MenuAction, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss_overlay(window, cx);
        let ui = self.session.as_ref().map(Session::ui);
        let inventory = ui.as_ref().and_then(|ui| ui.inventory.as_ref());
        let workspace = inventory.and_then(|inventory| {
            inventory.workspaces.iter().find(|workspace| {
                self.selected
                    .as_ref()
                    .is_some_and(|selected| selected.workspace_id == workspace.workspace_id)
            })
        });
        match action {
            MenuAction::Rename => self.open_naming(window, cx),
            MenuAction::NewTab => self.create(Target::TabLabel(format!(
                "Shell {}",
                workspace.map_or(1, |workspace| workspace.tabs.len() + 1)
            ))),
            MenuAction::NewWorkspace => {
                self.new_workspace(inventory.map_or(0, |inventory| inventory.workspaces.len()))
            }
            MenuAction::SplitRight => self.create(Target::SplitAxis(SplitAxis::Horizontal as i32)),
            MenuAction::SplitDown => self.create(Target::SplitAxis(SplitAxis::Vertical as i32)),
            MenuAction::Focus => self.focus_mode = !self.focus_mode,
            MenuAction::Zoom => self.zoom_pane = !self.zoom_pane,
            MenuAction::Settings => self.open_management(management::Page::Connections, window, cx),
            MenuAction::DisconnectOthers => self.disconnect_others(window, cx),
            MenuAction::Detach => {
                self.lifetime.stop();
                window.focus(&self.setup_focus);
            }
            MenuAction::ClosePane | MenuAction::CloseTab | MenuAction::CloseWorkspace => {
                if let Some(selected) = &self.selected {
                    let (title, tab_id, pane_id) = match action {
                        MenuAction::ClosePane => (
                            "End this terminal?",
                            Some(selected.tab_id.clone()),
                            Some(selected.pane_id.clone()),
                        ),
                        MenuAction::CloseTab => (
                            "End this tab and its terminals?",
                            Some(selected.tab_id.clone()),
                            None,
                        ),
                        _ => ("End this workspace and its terminals?", None, None),
                    };
                    self.confirm_close(
                        title,
                        CloseView {
                            workspace_id: selected.workspace_id.clone(),
                            tab_id,
                            pane_id,
                            ..Default::default()
                        },
                        window,
                        cx,
                    );
                }
            }
        }
        cx.notify();
    }

    fn toolbar(
        &self,
        ui: Option<&Ui>,
        workspace: Option<&WorkspaceState>,
        cx: &mut Context<Self>,
    ) -> Div {
        let location = self
            .destination
            .as_ref()
            .map_or("Local", Destination::label);
        let title = workspace.map_or_else(
            || format!("{location} ▾"),
            |workspace| format!("{location} / {} ▾", workspace.label),
        );
        let needs_input = ui
            .filter(|ui| ui.connected && ui.inventory_ready)
            .and_then(|ui| ui.inventory.as_ref())
            .map_or(0, |inventory| {
                inventory
                    .agents
                    .iter()
                    .filter(|agent| {
                        agent.terminal_available && agent.badge == AgentBadge::NeedsInput as i32
                    })
                    .count()
            });
        div()
            .flex()
            .flex_none()
            .items_center()
            .h(px(40.))
            .gap(px(5.))
            .px(px(8.))
            .bg(rgb(PANEL))
            .child(
                div().flex_none().max_w(px(250.)).overflow_hidden().child(
                    button("workspaces", title, self.switcher_open).on_click(
                        cx.listener(|view, _, window, cx| view.toggle_switcher(window, cx)),
                    ),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .when_some(ui.zip(workspace), |bar, (ui, workspace)| {
                        bar.child(self.tabs(workspace, ui, cx))
                    }),
            )
            .when(ui.is_some_and(Ui::can_write), |bar| {
                bar.child(
                    button("create-menu", "+", self.menu_open && self.menu_create).on_click(
                        cx.listener(|view, _, window, cx| view.toggle_menu(true, window, cx)),
                    ),
                )
            })
            .when(ui.is_some_and(|ui| ui.connected), |bar| {
                bar.child(
                    button(
                        "agents",
                        if needs_input > 0 {
                            format!("Agents · {needs_input} need input")
                        } else {
                            "Agents".into()
                        },
                        needs_input > 0,
                    )
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.dismiss_overlay(window, cx);
                        if let Some(session) = &view.session {
                            session.open_agent_deck(view.agent_deck.clone());
                        }
                    })),
                )
            })
            .child(
                button("workspace-menu", "⋯", self.menu_open && !self.menu_create).on_click(
                    cx.listener(|view, _, window, cx| view.toggle_menu(false, window, cx)),
                ),
            )
    }

    fn tabs(&self, workspace: &WorkspaceState, ui: &Ui, cx: &mut Context<Self>) -> Stateful<Div> {
        let selected = ui.selected.as_ref().map(|view| view.tab_id.as_str());
        let inventory = ui.inventory.as_ref();
        div()
            .id("daemon-tabs")
            .flex()
            .flex_1()
            .min_w_0()
            .gap(px(3.))
            .overflow_x_scroll()
            .children(workspace.tabs.iter().filter_map(|tab| {
                let (target, open) = self.choose(workspace, tab, inventory?)?;
                let active = selected == Some(tab.tab_id.as_str());
                let attention = ui.connected
                    && ui.inventory_ready
                    && inventory?.agents.iter().any(|agent| {
                        agent.terminal_available
                            && agent.badge == AgentBadge::NeedsInput as i32
                            && agent
                                .location
                                .as_ref()
                                .is_some_and(|location| location.tab_id == tab.tab_id)
                    });
                let title = if attention {
                    format!("{} !", tab.label)
                } else {
                    tab.label.clone()
                };
                Some(
                    div()
                        .id(SharedString::from(tab.tab_id.clone()))
                        .flex()
                        .flex_none()
                        .items_center()
                        .rounded_md()
                        .bg(rgb(if active { ACTIVE_FILL } else { PANEL }))
                        .max_w(px(240.))
                        .child(div().min_w_0().overflow_hidden().child(
                            button("select-tab", title, active).on_click(cx.listener(
                                move |view, _, window, cx| {
                                    view.dismiss_overlay(window, cx);
                                    view.select(target.clone(), open, window, cx);
                                },
                            )),
                        ))
                        .when(ui.can_close(), |row| {
                            row.child(Self::close_button(
                                "close-tab",
                                "End this tab and its terminals?",
                                CloseView {
                                    workspace_id: workspace.workspace_id.clone(),
                                    tab_id: Some(tab.tab_id.clone()),
                                    ..Default::default()
                                },
                                cx,
                            ))
                        }),
                )
            }))
    }

    fn action_menu(&self, cx: &mut Context<Self>) -> Div {
        let panel = div()
            .id("action-menu")
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .flex()
            .flex_col()
            .w(px(350.))
            .max_w_full()
            .p(px(7.))
            .gap(px(2.))
            .rounded_md()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .children(self.menu_items().into_iter().enumerate().map(
                |(index, (action, label, shortcut))| {
                    div()
                        .id(("menu-action", index))
                        .flex()
                        .justify_between()
                        .items_center()
                        .gap(px(12.))
                        .px(px(10.))
                        .py(px(6.))
                        .rounded_md()
                        .bg(rgb(if index == self.menu_index {
                            ACTIVE_FILL
                        } else {
                            PANEL
                        }))
                        .text_size(px(13.))
                        .text_color(rgb(TEXT))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(ACTIVE_FILL)))
                        .child(label)
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(MUTED))
                                .child(shortcut),
                        )
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.run_menu_action(action, window, cx)
                        }))
                },
            ));
        self.overlay(panel, true, cx)
    }

    fn status_message(&self, ui: Option<&Ui>) -> Option<String> {
        if self.host_switch.is_some() {
            return Some("Switching connection…".into());
        }
        if let Some(error) = &self.error {
            return Some(error.clone());
        }
        let ui = ui?;
        if ui.connected && !ui.inventory_ready {
            return Some("Syncing workspaces…".into());
        }
        if ui.changing {
            return Some("Updating terminal view…".into());
        }
        if !ui.connected || ui.message != "Attached to Corvid daemon" {
            return Some(ui.message.clone());
        }
        None
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = self.session.as_ref().map(Session::ui);
        let inventory = ui.as_ref().and_then(|ui| ui.inventory.as_ref());
        let workspace = ui
            .as_ref()
            .and_then(|ui| ui.selected.as_ref())
            .and_then(|selected| {
                inventory?
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == selected.workspace_id)
            });
        let status = self.status_message(ui.as_ref());
        let content = if let Some((ui, inventory)) = ui.as_ref().zip(inventory) {
            self.terminal_layout(ui, inventory, cx).into_any_element()
        } else if ui.as_ref().is_some_and(|ui| ui.connected) {
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
            .relative()
            .track_focus(&self.setup_focus)
            .capture_key_down(cx.listener(Self::key_down))
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BACKGROUND))
            .when(!self.focus_mode, |root| {
                root.child(self.toolbar(ui.as_ref(), workspace, cx))
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(content),
            )
            .when(self.focus_mode, |root| {
                root.child(div().absolute().top(px(5.)).right(px(8.)).child(
                    button("leave-focus", "Show tabs", false).on_click(cx.listener(
                        |view, _, _, cx| {
                            view.focus_mode = false;
                            cx.notify();
                        },
                    )),
                ))
            })
            .when_some(status, |root, message| {
                root.child(
                    div()
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .right_0()
                        .px(px(12.))
                        .py(px(5.))
                        .bg(rgb(PANEL))
                        .text_size(px(12.))
                        .text_color(rgb(TEXT))
                        .child(message),
                )
            })
            .when(self.switcher_open, |root| {
                root.child(self.switcher(window, cx))
            })
            .when(self.menu_open, |root| root.child(self.action_menu(cx)))
            .when(self.naming.is_some(), |root| {
                root.child(self.naming_panel(cx))
            })
    }
}
