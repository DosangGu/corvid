use super::*;
use gpui::MouseButton;

#[derive(Clone)]
pub(super) struct Choice {
    destination: Destination,
    workspace: String,
    label: String,
    live: bool,
    current: bool,
}

impl WorkspaceView {
    pub(super) fn dismiss_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.naming = None;
        self.switcher_open = false;
        self.menu_open = false;
        self.focus_terminal(window, cx);
        cx.notify();
    }

    pub(super) fn toggle_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher_open {
            self.dismiss_overlay(window, cx);
            return;
        }
        self.menu_open = false;
        self.switcher_open = true;
        self.switcher_query.clear();
        self.switcher_index = self
            .workspace_choices()
            .iter()
            .position(|choice| choice.current)
            .unwrap_or(0);
        window.focus(&self.setup_focus);
        cx.notify();
    }

    fn workspace_choices(&self) -> Vec<Choice> {
        let query = self.switcher_query.to_lowercase();
        let mut choices = Vec::new();
        for (destination, known) in &self.known_sessions {
            for workspace in &known.inventory.workspaces {
                if !format!("{} {}", destination.label(), workspace.label)
                    .to_lowercase()
                    .contains(&query)
                {
                    continue;
                }
                let current = self.destination.as_ref() == Some(destination)
                    && self
                        .selected
                        .as_ref()
                        .is_some_and(|selected| selected.workspace_id == workspace.workspace_id);
                choices.push(Choice {
                    destination: destination.clone(),
                    workspace: workspace.workspace_id.clone(),
                    label: workspace.label.clone(),
                    live: known.live,
                    current,
                });
            }
        }
        choices.sort_by(|a, b| {
            a.destination
                .label()
                .cmp(b.destination.label())
                .then(a.label.cmp(&b.label))
                .then(a.workspace.cmp(&b.workspace))
        });
        choices
    }

    fn activate_workspace_choice(
        &mut self,
        choice: &Choice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_overlay(window, cx);
        self.open_workspace(
            choice.destination.clone(),
            choice.workspace.clone(),
            window,
            cx,
        );
    }

    pub(super) fn switcher_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = &event.keystroke;
        let choices = self.workspace_choices();
        match key.key.as_str() {
            "escape" => self.dismiss_overlay(window, cx),
            "up" => self.switcher_index = self.switcher_index.saturating_sub(1),
            "down" => {
                self.switcher_index = (self.switcher_index + 1).min(choices.len().saturating_sub(1))
            }
            "enter" => {
                if let Some(choice) = choices.get(self.switcher_index) {
                    self.activate_workspace_choice(choice, window, cx);
                }
            }
            "backspace" => {
                self.switcher_query.pop();
                self.switcher_index = 0;
            }
            "v" if key.modifiers.control => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.append_query(&text);
                }
            }
            _ if !key.modifiers.control && !key.modifiers.alt && !key.modifiers.platform => {
                if let Some(text) = &key.key_char {
                    self.append_query(text);
                }
            }
            _ => {}
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn append_query(&mut self, text: &str) {
        for ch in text.chars().filter(|ch| !ch.is_control()) {
            if self.switcher_query.len() + ch.len_utf8() > 256 {
                break;
            }
            self.switcher_query.push(ch);
        }
        self.switcher_index = 0;
    }

    pub(super) fn switcher(&self, cx: &mut Context<Self>) -> Div {
        let choices = self.workspace_choices();
        let selected = self.switcher_index.min(choices.len().saturating_sub(1));
        let start = selected.saturating_sub(5);
        let total = choices.len();
        let choice_actions = choices.get(selected).filter(|choice| choice.live).cloned();
        let count = self
            .session
            .as_ref()
            .map(Session::ui)
            .and_then(|ui| ui.inventory)
            .map_or(0, |inventory| inventory.workspaces.len());
        let panel =
            div()
                .id("workspace-picker")
                .occlude()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .flex()
                .flex_col()
                .w(px(440.))
                .max_w_full()
                .p(px(10.))
                .gap(px(5.))
                .rounded_md()
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(BORDER))
                .child(
                    div()
                        .px(px(10.))
                        .py(px(8.))
                        .bg(rgb(BACKGROUND))
                        .rounded_md()
                        .text_color(rgb(if self.switcher_query.is_empty() {
                            MUTED
                        } else {
                            TEXT
                        }))
                        .child(if self.switcher_query.is_empty() {
                            "Search workspaces…".to_owned()
                        } else {
                            format!("{}▏", self.switcher_query)
                        }),
                )
                .child(
                    div()
                        .px(px(10.))
                        .py(px(4.))
                        .text_size(px(12.))
                        .text_color(rgb(MUTED))
                        .child(format!(
                            "Workspaces · {total}   ↑/↓ select · Enter open · Esc return"
                        )),
                )
                .when(choices.is_empty(), |panel| {
                    panel.child(
                        div()
                            .p(px(10.))
                            .text_color(rgb(MUTED))
                            .child("No matching workspaces."),
                    )
                })
                .children(choices.into_iter().enumerate().skip(start).take(8).map(
                    |(index, choice)| {
                        let title = format!("{} / {}", choice.destination.label(), choice.label);
                        let status = if choice.current {
                            "Current"
                        } else if choice.live {
                            "Live"
                        } else {
                            "Cached"
                        };
                        div()
                            .id(("workspace-choice", index))
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .px(px(10.))
                            .py(px(7.))
                            .rounded_md()
                            .bg(rgb(if index == selected {
                                ACTIVE_FILL
                            } else {
                                PANEL
                            }))
                            .text_color(rgb(TEXT))
                            .text_size(px(13.))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(ACTIVE_FILL)))
                            .child(div().flex_1().min_w_0().truncate().child(title))
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(12.))
                                    .text_color(rgb(MUTED))
                                    .child(status),
                            )
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.activate_workspace_choice(&choice, window, cx)
                            }))
                    },
                ))
                .when_some(choice_actions, |panel, choice| {
                    panel.child(self.choice_actions(choice, cx))
                })
                .child(
                    div()
                        .mt(px(6.))
                        .border_t_1()
                        .border_color(rgb(BORDER))
                        .pt(px(6.))
                        .flex()
                        .justify_between()
                        .when(
                            self.session
                                .as_ref()
                                .map(Session::ui)
                                .is_some_and(|ui| ui.can_write()),
                            |row| {
                                row.child(
                                    button("picker-new-workspace", "+ New workspace", false)
                                        .on_click(cx.listener(move |view, _, window, cx| {
                                            view.dismiss_overlay(window, cx);
                                            view.new_workspace(count);
                                        })),
                                )
                            },
                        )
                        .child(button("picker-settings", "Connections…", false).on_click(
                            cx.listener(|view, _, window, cx| {
                                view.open_management(management::Page::Connections, window, cx)
                            }),
                        )),
                );
        self.overlay(panel, false, cx)
    }

    pub(super) fn overlay(
        &self,
        panel: impl IntoElement,
        right: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .pt(px(if self.focus_mode { 10. } else { 44. }))
            .px(px(8.))
            .when(right, |layer| layer.justify_end())
            .child(
                div()
                    .id("overlay-dismiss")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, _, window, cx| {
                            view.dismiss_overlay(window, cx);
                            cx.stop_propagation();
                        }),
                    ),
            )
            .child(panel)
    }

    fn choice_actions(&self, choice: Choice, cx: &mut Context<Self>) -> Div {
        let observation = choice.clone();
        div()
            .flex()
            .gap(px(6.))
            .pt(px(4.))
            .child(
                button("picker-new-tab", "Open in new tab", false).on_click(cx.listener(
                    move |view, _, window, cx| {
                        let Some(known) = view.known_sessions.get(&choice.destination) else {
                            return;
                        };
                        let Some(workspace) = known
                            .inventory
                            .workspaces
                            .iter()
                            .find(|workspace| workspace.workspace_id == choice.workspace)
                        else {
                            return;
                        };
                        let target = AttachTarget::Workspace {
                            id: choice.workspace.clone(),
                            host_id: known.host_id.clone(),
                            new_tab: Some(format!("Shell {}", workspace.tabs.len() + 1)),
                        };
                        view.dismiss_overlay(window, cx);
                        view.switch_to(choice.destination.clone(), target, window, cx);
                    },
                )),
            )
            .child(
                button("picker-observe", "Observe", false).on_click(cx.listener(
                    move |view, _, window, cx| {
                        view.dismiss_overlay(window, cx);
                        view.observe(
                            observation.destination.clone(),
                            Some(&observation.workspace),
                            window,
                            cx,
                        );
                    },
                )),
            )
    }
}
