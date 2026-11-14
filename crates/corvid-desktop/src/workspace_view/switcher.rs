use super::*;
use gpui::{MouseButton, ScrollHandle};

const MAX_HOST_LOOKUPS: usize = 3;

#[derive(Default)]
pub(super) struct Picker {
    query: String,
    expanded: HashMap<Destination, bool>,
    selected: Option<RowKey>,
    scroll: ScrollHandle,
}

#[derive(Clone, PartialEq, Eq)]
enum RowKey {
    Machine(Destination),
    Workspace {
        destination: Destination,
        host_id: String,
        workspace_id: String,
    },
    Connect(Destination),
}

impl RowKey {
    fn destination(&self) -> &Destination {
        match self {
            Self::Machine(destination) | Self::Connect(destination) => destination,
            Self::Workspace { destination, .. } => destination,
        }
    }

    fn element_id(&self) -> SharedString {
        let machine = machine_label(self.destination());
        match self {
            Self::Machine(_) => format!("machine-{machine}"),
            Self::Connect(_) => format!("connect-{machine}"),
            Self::Workspace {
                host_id,
                workspace_id,
                ..
            } => {
                format!("workspace-{machine}-{host_id}-{workspace_id}")
            }
        }
        .into()
    }
}

struct Row {
    key: RowKey,
    title: String,
    status: &'static str,
    expanded: bool,
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
        } else {
            self.show_switcher(window, cx);
        }
    }

    pub(super) fn show_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu_open = false;
        self.switcher_open = true;
        self.switcher = Picker::default();
        self.refresh_hosts(cx);
        #[cfg(windows)]
        self.refresh_wsl(cx);

        let destination = self.destination.clone().unwrap_or(Destination::Local);
        self.load_machine(destination.clone(), cx);
        let rows = self.picker_rows();
        let machine = RowKey::Machine(destination);
        if let Some(index) = rows
            .iter()
            .position(|row| row.status == "Current")
            .or_else(|| rows.iter().position(|row| row.key == machine))
        {
            self.select_row(&rows, index);
        }

        window.focus(&self.setup_focus);
        cx.notify();
    }

    fn machine_expanded(&self, destination: &Destination) -> bool {
        self.switcher
            .expanded
            .get(destination)
            .copied()
            .unwrap_or_else(|| {
                self.destination.as_ref() == Some(destination)
                    || self
                        .known_sessions
                        .get(destination)
                        .is_some_and(|known| known.inventory.is_some())
            })
    }

    fn picker_rows(&self) -> Vec<Row> {
        let ui = self.session.as_ref().map(Session::ui);
        let query = self.switcher.query.to_lowercase();
        let mut destinations = vec![Destination::Local];
        destinations.extend(self.ssh_destinations());
        #[cfg(windows)]
        destinations.extend(self.wsl_destinations());

        let mut rows = Vec::new();
        for destination in destinations {
            let label = machine_label(&destination);
            let current = self.destination.as_ref() == Some(&destination);
            let current_ui = ui.as_ref().filter(|_| current);
            let known = self.known_sessions.get(&destination);
            let inventory = known.and_then(|known| known.inventory.as_deref());
            let error = self.lookup_errors.get(&destination).copied();
            let loading = self.host_lookups.contains_key(&destination)
                || current_ui.is_some_and(|ui| ui.changing && !ui.connected);
            let live = known.is_some_and(|known| known.live);
            let mut workspaces: Vec<_> = inventory
                .into_iter()
                .flat_map(|inventory| &inventory.workspaces)
                .filter(|workspace| matches_query(&query, &label, Some(&workspace.label)))
                .collect();

            let machine_matches = matches_query(&query, &label, None);
            if !machine_matches && workspaces.is_empty() {
                continue;
            }
            workspaces.sort_unstable_by(|left, right| {
                left.label
                    .cmp(&right.label)
                    .then(left.workspace_id.cmp(&right.workspace_id))
            });

            let expanded = self.machine_expanded(&destination)
                || (!query.is_empty()
                    && !workspaces.is_empty()
                    && !self.switcher.expanded.contains_key(&destination));
            let status = if loading {
                "Loading…"
            } else if current_ui.is_some_and(|ui| ui.connected) {
                "Connected"
            } else if live && inventory.is_some_and(|inventory| inventory.workspaces.is_empty()) {
                "No workspaces"
            } else if live {
                "Live · detached"
            } else if let Some(error) = error {
                error
            } else if inventory.is_some() {
                "Cached"
            } else {
                "Workspaces not loaded"
            };
            rows.push(Row {
                key: RowKey::Machine(destination.clone()),
                title: format!("{} {label}", if expanded { "▾" } else { "▸" }),
                status,
                expanded,
            });
            if !expanded {
                continue;
            }

            if let Some(known) = known {
                for workspace in workspaces {
                    let selected = current
                        && current_ui.is_some_and(|ui| {
                            ui.host_id.as_ref() == Some(&known.host_id) && ui.connected
                        })
                        && self.selected.as_ref().is_some_and(|selected| {
                            selected.workspace_id == workspace.workspace_id
                        });
                    rows.push(Row {
                        key: RowKey::Workspace {
                            destination: destination.clone(),
                            host_id: known.host_id.clone(),
                            workspace_id: workspace.workspace_id.clone(),
                        },
                        title: workspace.label.clone(),
                        expanded: false,
                        status: if selected {
                            "Current"
                        } else if known.live {
                            ""
                        } else {
                            "Cached"
                        },
                    });
                }
            }
            if machine_matches
                && !loading
                && !current_ui.is_some_and(|ui| ui.connected)
                && inventory.is_none_or(|inventory| inventory.workspaces.is_empty())
            {
                rows.push(Row {
                    key: RowKey::Connect(destination),
                    title: "Open terminal…".into(),
                    status: "",
                    expanded: false,
                });
            }
        }
        rows
    }

    fn load_machine(&mut self, destination: Destination, cx: &mut Context<Self>) {
        let known = self.known_sessions.get(&destination);
        let busy = self.destination.as_ref() == Some(&destination)
            && self
                .session
                .as_ref()
                .map(Session::ui)
                .is_some_and(|ui| ui.connected || ui.changing);
        if busy
            || known.is_some_and(|known| known.live)
            || self.host_lookups.contains_key(&destination)
        {
            return;
        }
        if self.host_lookups.len() >= MAX_HOST_LOOKUPS {
            self.lookup_errors
                .insert(destination, "Lookup limit reached · retry");
            return;
        }

        let hint = known.map(|known| (known.endpoint.clone(), known.host_id.clone()));
        let Ok(result) = host_watch::lookup(destination.clone(), hint) else {
            self.lookup_errors
                .insert(destination, "Could not load workspaces");
            return;
        };

        self.lookup_errors.remove(&destination);
        let machine = destination.clone();
        let task = cx.spawn(async move |view, cx| {
            let result = result.await.unwrap_or(Err("Could not load workspaces"));
            let _ = view.update(cx, |view, cx| {
                view.host_lookups.remove(&machine);
                if !view
                    .known_sessions
                    .get(&machine)
                    .is_some_and(|known| known.live)
                {
                    match result {
                        Ok(known) => {
                            view.known_sessions.insert(machine.clone(), known);
                            view.watch_host(machine, cx);
                        }
                        Err(error) => {
                            view.lookup_errors.insert(machine, error);
                        }
                    }
                }
                cx.notify();
            });
        });
        self.host_lookups.insert(destination, task);
    }

    fn expand_machine(&mut self, destination: Destination, expanded: bool, cx: &mut Context<Self>) {
        self.switcher.expanded.insert(destination.clone(), expanded);
        if expanded {
            self.load_machine(destination, cx);
        }
        cx.notify();
    }

    fn activate_row(
        &mut self,
        key: RowKey,
        expanded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match key {
            RowKey::Machine(destination) => {
                self.switcher.selected = Some(RowKey::Machine(destination.clone()));
                self.expand_machine(destination, !expanded, cx);
            }
            RowKey::Workspace {
                destination,
                host_id,
                workspace_id,
            } => {
                self.dismiss_overlay(window, cx);
                self.open_workspace(destination, host_id, workspace_id, window, cx);
            }
            RowKey::Connect(destination) => {
                self.dismiss_overlay(window, cx);
                self.switch_to(destination, AttachTarget::FreshDefault, window, cx);
            }
        }
    }

    fn select_row(&mut self, rows: &[Row], index: usize) {
        self.switcher.selected = rows.get(index).map(|row| row.key.clone());
        self.switcher.scroll.scroll_to_item(index);
    }

    fn filter_changed(&mut self) {
        self.switcher.expanded.clear();
        let rows = self.picker_rows();
        self.switcher.scroll.scroll_to_top_of_item(0);
        let index = rows
            .iter()
            .position(|row| matches!(row.key, RowKey::Workspace { .. }));
        self.select_row(&rows, index.unwrap_or(0));
    }

    pub(super) fn switcher_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = &event.keystroke;
        let rows = self.picker_rows();
        let index = self.switcher.selected_index(&rows);
        let row = index.and_then(|index| rows.get(index));
        match key.key.as_str() {
            "escape" => self.dismiss_overlay(window, cx),
            "up" => self.select_row(&rows, index.unwrap_or(0).saturating_sub(1)),
            "down" => {
                let next = index.map_or(0, |index| index + 1);
                self.select_row(&rows, next.min(rows.len().saturating_sub(1)));
            }
            "enter" => {
                if let Some(row) = row {
                    self.activate_row(row.key.clone(), row.expanded, window, cx);
                }
            }
            "left" | "right" => {
                if let Some(row) = row {
                    let destination = row.key.destination();
                    if key.key == "left" && !matches!(row.key, RowKey::Machine(_)) {
                        if let Some(index) = rows
                            .iter()
                            .position(|row| row.key == RowKey::Machine(destination.clone()))
                        {
                            self.select_row(&rows, index);
                        }
                    } else if matches!(row.key, RowKey::Machine(_)) {
                        self.expand_machine(destination.clone(), key.key == "right", cx);
                    }
                }
            }
            "backspace" => {
                self.switcher.query.pop();
                self.filter_changed();
            }
            "v" if key.modifiers.control => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.switcher.append_query(&text);
                    self.filter_changed();
                }
            }
            _ if !key.modifiers.control && !key.modifiers.alt && !key.modifiers.platform => {
                if let Some(text) = &key.key_char {
                    self.switcher.append_query(text);
                    self.filter_changed();
                }
            }
            _ => {}
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn switcher(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let rows = self.picker_rows();
        let selected = self.switcher.selected_index(&rows);
        let ui = self.session.as_ref().map(Session::ui);
        let note = if self.host_scan.is_some() {
            Some("Reading SSH configuration…")
        } else if self.hosts.is_err() {
            Some("Could not read SSH configuration")
        } else if rows.is_empty() {
            Some("No matching machines or workspaces")
        } else {
            None
        };
        let panel = div()
            .id("workspace-picker")
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .flex()
            .flex_col()
            .w(px(480.))
            .max_w_full()
            .max_h(window.viewport_size().height - px(54.))
            .p(px(10.))
            .gap(px(5.))
            .rounded_md()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex_none()
                    .px(px(10.))
                    .py(px(8.))
                    .bg(rgb(BACKGROUND))
                    .rounded_md()
                    .text_color(rgb(if self.switcher.query.is_empty() {
                        MUTED
                    } else {
                        TEXT
                    }))
                    .child(if self.switcher.query.is_empty() {
                        "Search machines and workspaces…".to_owned()
                    } else {
                        format!("{}▏", self.switcher.query)
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(10.))
                    .py(px(4.))
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child("↑↓ Move   ←→ Expand or collapse   Enter Select   Esc Return"),
            )
            .when_some(note, |panel, note| {
                panel.child(
                    div()
                        .flex_none()
                        .px(px(10.))
                        .text_size(px(12.))
                        .text_color(rgb(MUTED))
                        .child(note),
                )
            })
            .child(
                div()
                    .id("picker-rows")
                    .flex()
                    .flex_col()
                    .flex_shrink()
                    .min_h_0()
                    .max_h(px(330.))
                    .overflow_y_scroll()
                    .track_scroll(&self.switcher.scroll)
                    .children(
                        rows.iter()
                            .enumerate()
                            .map(|(index, row)| self.picker_row(row, selected == Some(index), cx)),
                    ),
            )
            .child(self.picker_actions(
                selected.and_then(|index| rows.get(index)),
                ui.as_ref(),
                cx,
            ));
        self.overlay(panel, false, cx)
    }

    fn picker_row(&self, row: &Row, selected: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        let machine = matches!(row.key, RowKey::Machine(_));
        let key = row.key.clone();
        let expanded = row.expanded;
        div()
            .id(key.element_id())
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .pl(px(if machine { 10. } else { 29. }))
            .pr(px(10.))
            .py(px(7.))
            .rounded_md()
            .bg(rgb(if selected {
                ACTIVE_FILL
            } else if machine {
                BACKGROUND
            } else {
                PANEL
            }))
            .text_color(rgb(TEXT))
            .text_size(px(13.))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(ACTIVE_FILL)))
            .child(div().flex_1().min_w_0().truncate().child(row.title.clone()))
            .when(!row.status.is_empty(), |item| {
                item.child(
                    div()
                        .flex_none()
                        .text_size(px(12.))
                        .text_color(rgb(if row.status == "Current" {
                            ACTIVE
                        } else {
                            MUTED
                        }))
                        .child(row.status),
                )
            })
            .on_click(cx.listener(move |view, _, window, cx| {
                view.activate_row(key.clone(), expanded, window, cx)
            }))
    }

    fn picker_actions(&self, row: Option<&Row>, ui: Option<&Ui>, cx: &mut Context<Self>) -> Div {
        let mut actions = div()
            .flex()
            .flex_none()
            .gap(px(6.))
            .pt(px(6.))
            .border_t_1()
            .border_color(rgb(BORDER));
        if let Some(RowKey::Workspace {
            destination,
            host_id,
            workspace_id,
        }) = row.map(|row| &row.key)
            && self
                .known_sessions
                .get(destination)
                .is_some_and(|known| known.live)
        {
            let (destination, host_id, workspace_id) =
                (destination.clone(), host_id.clone(), workspace_id.clone());
            actions = actions.child(button("picker-new-tab", "Open in new tab", false).on_click(
                cx.listener(move |view, _, window, cx| {
                    let Some(workspace) = view
                        .known_sessions
                        .get(&destination)
                        .filter(|known| known.live && known.host_id == host_id)
                        .and_then(|known| known.inventory.as_ref())
                        .and_then(|inventory| {
                            inventory
                                .workspaces
                                .iter()
                                .find(|workspace| workspace.workspace_id == workspace_id)
                        })
                    else {
                        return;
                    };
                    let target = AttachTarget::Workspace {
                        id: workspace_id.clone(),
                        host_id: host_id.clone(),
                        new_tab: Some(format!("Shell {}", workspace.tabs.len() + 1)),
                    };
                    view.dismiss_overlay(window, cx);
                    view.switch_to(destination.clone(), target, window, cx);
                }),
            ));
        }
        if let Some(ui) = ui.filter(|ui| ui.can_write()) {
            let count = ui
                .inventory
                .as_ref()
                .map_or(0, |inventory| inventory.workspaces.len());
            actions = actions.child(
                button("picker-new-workspace", "+ New workspace", false).on_click(cx.listener(
                    move |view, _, window, cx| {
                        view.dismiss_overlay(window, cx);
                        view.new_workspace(count);
                    },
                )),
            );
        }
        actions.child(
            button("picker-settings", "Connections…", false).on_click(cx.listener(
                |view, _, window, cx| {
                    view.open_management(management::Page::Connections, window, cx)
                },
            )),
        )
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
}

impl Picker {
    fn selected_index(&self, rows: &[Row]) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        rows.iter().position(|row| row.key == *selected)
    }

    fn append_query(&mut self, text: &str) {
        for ch in text.chars().filter(|ch| !ch.is_control()) {
            if self.query.len() + ch.len_utf8() > 256 {
                break;
            }
            self.query.push(ch);
        }
    }
}

fn machine_label(destination: &Destination) -> String {
    match destination {
        Destination::Local => "Local".into(),
        Destination::Ssh(alias) => format!("SSH · {}", alias.as_str()),
        #[cfg(windows)]
        Destination::Wsl(distribution) => format!("WSL · {}", distribution.as_str()),
    }
}

fn matches_query(query: &str, machine: &str, workspace: Option<&str>) -> bool {
    let machine = machine.to_lowercase();
    let workspace = workspace.map(str::to_lowercase);
    query.split_whitespace().all(|word| {
        machine.contains(word) || workspace.as_ref().is_some_and(|name| name.contains(word))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_workspace_does_not_follow_a_reordered_or_replaced_row() {
        let work = Destination::Ssh(SshAlias::new("work".into()).unwrap());
        let lab = Destination::Ssh(SshAlias::new("lab".into()).unwrap());
        let workspace = |destination, host_id: &str| Row {
            key: RowKey::Workspace {
                destination,
                host_id: host_id.into(),
                workspace_id: "same-id".into(),
            },
            title: "Main".into(),
            status: "",
            expanded: false,
        };
        let selected = workspace(work.clone(), "original");
        let picker = Picker {
            selected: Some(selected.key.clone()),
            ..Picker::default()
        };
        assert_eq!(
            picker.selected_index(&[workspace(lab.clone(), "lab"), selected]),
            Some(1)
        );
        assert_eq!(
            picker.selected_index(&[workspace(lab, "lab"), workspace(work, "replacement")]),
            None
        );
    }
}
