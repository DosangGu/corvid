use super::*;
use corvid_core::layout::Axis;
use corvid_core::navigation::{Direction, Rect, WorkspaceCommand, pane_mode_hint, tab_mode_hint};
use corvid_core::shortcuts::dubeolsik_key;
use corvid_protocol::wire::reorder_tab::Placement;
use corvid_protocol::wire::{SplitPane, TabStep};

pub(super) enum Overlay {
    Commands {
        query: String,
        index: usize,
    },
    Tabs {
        workspace_id: String,
        tab_id: String,
    },
    Panes(ViewSelection),
}

impl Overlay {
    pub(super) fn highlighted_tab(&self, workspace: &str) -> Option<&str> {
        match self {
            Self::Tabs {
                workspace_id,
                tab_id,
            } if workspace_id == workspace => Some(tab_id),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum Command {
    Workspace(WorkspaceCommand),
    NewWorkspace,
    NewWindow,
    CloseWindow,
    Quit,
    Settings,
    Agents,
    Reconnect,
    OpenTerminal,
    History(bool),
    Zoom,
}

impl Command {
    fn label(self) -> &'static str {
        match self {
            Self::Workspace(action) => action.label(),
            Self::NewWorkspace => "New workspace",
            Self::NewWindow => "New window",
            Self::CloseWindow => "Close window",
            Self::Quit => "Quit Corvid Desktop",
            Self::Settings => "Settings",
            Self::Agents => "Agents",
            Self::Reconnect => "Reconnect",
            Self::OpenTerminal => "Open terminal",
            Self::History(false) => "Browse terminal history",
            Self::History(true) => "Search terminal history",
            Self::Zoom => "Expand or restore active pane",
        }
    }
}

impl WorkspaceView {
    pub(super) fn navigation_target(&self) -> Option<(ViewSelection, Arc<Inventory>)> {
        let ui = self.session.as_ref()?.ui();
        if !ui.can_write() || ui.changing {
            return None;
        }
        Some((ui.selected?, ui.inventory?))
    }

    pub(super) fn pane_highlight(&self, id: &str) -> bool {
        matches!(&self.navigation, Some(Overlay::Panes(target)) if target.pane_id == id)
    }

    pub(super) fn show_commands(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_command(Command::Workspace(WorkspaceCommand::Commands), window, cx);
    }

    fn show_navigation(&mut self, overlay: Overlay, window: &mut Window, cx: &mut Context<Self>) {
        self.navigation = Some(overlay);
        window.focus(&self.setup_focus, cx);
        cx.notify();
    }

    fn commands(&self, query: &str) -> Vec<Command> {
        use Command::*;
        let mut items = vec![
            Workspace(WorkspaceCommand::Workspaces),
            NewWorkspace,
            NewWindow,
            CloseWindow,
            Quit,
            Settings,
        ];
        if self.session.as_ref().is_some_and(|s| s.ui().ssh_offline) {
            items.push(Reconnect);
        } else if self.terminal.is_none() {
            items.push(OpenTerminal);
        }
        if self.terminal.is_some() {
            items.extend([
                Agents,
                History(false),
                History(true),
                Zoom,
                Workspace(WorkspaceCommand::Detach),
            ]);
        }
        if self.navigation_target().is_some() {
            items.extend(
                WorkspaceCommand::ALL
                    .into_iter()
                    .filter(|action| {
                        matches!(
                            action,
                            WorkspaceCommand::Tabs
                                | WorkspaceCommand::Panes
                                | WorkspaceCommand::PreviousTab
                                | WorkspaceCommand::NextTab
                                | WorkspaceCommand::PreviousCommand
                                | WorkspaceCommand::CopyLastOutput
                                | WorkspaceCommand::RenameTab
                        )
                    })
                    .map(Workspace),
            );
        }
        matching(items, query, |item| item.label())
    }

    fn run_command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        use Command::*;
        self.dismiss_overlay(window, cx);
        match command {
            Workspace(WorkspaceCommand::Workspaces) => self.show_switcher(window, cx),
            Workspace(WorkspaceCommand::Tabs) => {
                if let Some((selected, _)) = self.navigation_target() {
                    self.show_navigation(
                        Overlay::Tabs {
                            workspace_id: selected.workspace_id,
                            tab_id: selected.tab_id,
                        },
                        window,
                        cx,
                    );
                }
            }
            Workspace(WorkspaceCommand::Panes) => {
                if let Some((selected, _)) = self.navigation_target() {
                    self.zoom_pane = false;
                    self.show_navigation(Overlay::Panes(selected), window, cx);
                }
            }
            Workspace(WorkspaceCommand::PreviousTab) => self.cycle_tab(true, window, cx),
            Workspace(WorkspaceCommand::NextTab) => self.cycle_tab(false, window, cx),
            Workspace(WorkspaceCommand::PreviousCommand) => {
                if !self
                    .session
                    .as_ref()
                    .is_some_and(|session| session.ui().command_history_supported)
                {
                    self.error = Some("This daemon does not support command history.".into());
                } else if let Some(terminal) = &self.terminal {
                    terminal.update(cx, |view, cx| view.jump_previous_command(cx));
                }
            }
            Workspace(WorkspaceCommand::CopyLastOutput) => {
                if !self
                    .session
                    .as_ref()
                    .is_some_and(|session| session.ui().command_history_supported)
                {
                    self.error = Some("This daemon does not support command history.".into());
                } else if let Some(terminal) = &self.terminal {
                    terminal.update(cx, |view, cx| view.copy_command_output(cx));
                }
            }
            Workspace(WorkspaceCommand::RenameTab) => self.open_naming(window, cx),
            Workspace(WorkspaceCommand::Commands) => self.show_navigation(
                Overlay::Commands {
                    query: String::new(),
                    index: 0,
                },
                window,
                cx,
            ),
            Workspace(WorkspaceCommand::Detach) => {
                self.lifetime.stop();
                window.focus(&self.setup_focus, cx);
            }
            NewWorkspace => self.new_workspace(
                self.destination.clone().unwrap_or(Destination::Local),
                window,
                cx,
            ),
            NewWindow => self.new_window(cx),
            CloseWindow => self.windows.close_window(window, cx),
            Quit => self.windows.quit(cx),
            Settings => self.open_management(management::Page::Connections, window, cx),
            Agents => {
                if let Some(session) = &self.session {
                    session.open_agent_deck(self.agent_deck.clone());
                }
            }
            Reconnect => self.reconnect_ssh(window, cx),
            OpenTerminal => self.switch_to(
                self.destination.clone().unwrap_or(Destination::Local),
                AttachTarget::Default,
                window,
                cx,
            ),
            History(search) => {
                if let Some(terminal) = &self.terminal {
                    terminal.update(cx, |view, cx| view.open_history_command(search, cx));
                }
            }
            Zoom => self.zoom_pane = !self.zoom_pane,
        }
        cx.notify();
    }

    pub(super) fn navigation_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = &event.keystroke;
        let leader = key.modifiers == gpui::Modifiers::control() && key.key == "]";
        if self.navigation.is_none() && !self.leader {
            if leader {
                cx.stop_propagation();
                if !event.is_held {
                    self.leader = true;
                    cx.notify();
                }
            }
            return;
        }
        cx.stop_propagation();
        let name = shortcuts::key(key);
        if event.is_held
            && !matches!(name, "left" | "right" | "up" | "down" | "backspace")
            && !matches!(
                (&self.navigation, name),
                (Some(Overlay::Tabs { .. }), "h" | "l" | "[" | "]")
                    | (Some(Overlay::Panes(_)), "h" | "j" | "k" | "l")
            )
        {
            return;
        }
        if name == "escape" {
            self.dismiss_overlay(window, cx);
            return;
        }
        if std::mem::take(&mut self.leader) {
            if leader {
                if let (Some(session), Some(view)) = (&self.session, &self.selected) {
                    for _ in 0..2 {
                        session.send(&view.pane_id, vec![corvid_core::KEY_LEADER], false);
                    }
                }
            } else if let Some(direction) = direction(name) {
                if let Some((view, _)) = self.navigation_target()
                    && let Some(target) = self.neighbor(&view, direction)
                {
                    self.select(target, false, window, cx);
                }
            } else if let Some(command) = workspace_command(key) {
                self.run_command(Command::Workspace(command), window, cx);
            }
            cx.notify();
            return;
        }
        let Some(overlay) = self.navigation.take() else {
            return;
        };
        if key.modifiers.modified() && matches!(overlay, Overlay::Tabs { .. } | Overlay::Panes(_)) {
            self.navigation = Some(overlay);
            return;
        }
        match overlay {
            Overlay::Commands {
                mut query,
                mut index,
            } => {
                let commands = self.commands(&query);
                if key.key == "enter" {
                    if let Some(&command) = commands.get(index) {
                        self.run_command(command, window, cx);
                        return;
                    }
                } else {
                    edit_list(key, &mut query, &mut index, commands.len());
                }
                self.navigation = Some(Overlay::Commands { query, index });
            }
            Overlay::Tabs {
                workspace_id,
                tab_id,
            } => {
                self.tab_mode_key(name, workspace_id, tab_id, window, cx);
            }
            Overlay::Panes(mut target) => {
                match name {
                    "enter" => {
                        self.dismiss_overlay(window, cx);
                        self.select(target, false, window, cx);
                        return;
                    }
                    "r" | "d" => {
                        self.dismiss_overlay(window, cx);
                        let axis = if name == "r" {
                            SplitAxis::Horizontal
                        } else {
                            SplitAxis::Vertical
                        } as i32;
                        if self.selected.as_ref() == Some(&target) {
                            self.create(Target::SplitAxis(axis));
                        } else if self
                            .session
                            .as_ref()
                            .is_some_and(|session| session.ui().split_target_supported)
                        {
                            self.create(Target::SplitPane(SplitPane {
                                pane_id: target.pane_id,
                                axis,
                            }));
                        } else {
                            self.error = Some(
                                "Focus that pane before splitting on this daemon version.".into(),
                            );
                            cx.notify();
                        }
                        return;
                    }
                    "x" => {
                        if self.close_highlighted(
                            CloseView {
                                workspace_id: target.workspace_id.clone(),
                                tab_id: Some(target.tab_id.clone()),
                                pane_id: Some(target.pane_id.clone()),
                                ..Default::default()
                            },
                            window,
                            cx,
                        ) {
                            return;
                        }
                    }
                    key => {
                        if let Some(next) = pane_direction(key)
                            .and_then(|direction| self.neighbor(&target, direction))
                        {
                            target = next;
                        }
                    }
                }
                self.navigation = Some(Overlay::Panes(target));
            }
        }
        cx.notify();
    }

    fn tab_mode_key(
        &mut self,
        key: &str,
        workspace_id: String,
        mut tab_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((selected, inventory)) = self.navigation_target() else {
            self.dismiss_overlay(window, cx);
            return;
        };
        let Some(workspace) = inventory.workspaces.iter().find(|workspace| {
            workspace.workspace_id == workspace_id && workspace_id == selected.workspace_id
        }) else {
            self.dismiss_overlay(window, cx);
            return;
        };
        let index = workspace.tabs.iter().position(|tab| tab.tab_id == tab_id);
        match key {
            "enter" => {
                if let Some((target, open)) = index
                    .and_then(|index| workspace.tabs.get(index))
                    .and_then(|tab| self.choose(workspace, tab, &inventory))
                {
                    self.dismiss_overlay(window, cx);
                    self.open_pane(target, open, window, cx);
                    return;
                }
            }
            "n" => {
                self.dismiss_overlay(window, cx);
                self.create(Target::TabLabel(String::new()));
                return;
            }
            "x" if index.is_some() => {
                if self.close_highlighted(
                    CloseView {
                        workspace_id: workspace_id.clone(),
                        tab_id: Some(tab_id.clone()),
                        ..Default::default()
                    },
                    window,
                    cx,
                ) {
                    return;
                }
            }
            "[" | "]" if index.is_some() => {
                if let Some(session) = &self.session
                    && let Some(view) = session.tab_reorder_view()
                {
                    let step = if key == "[" {
                        TabStep::Left
                    } else {
                        TabStep::Right
                    };
                    session.reorder_tab(
                        &view,
                        workspace_id.clone(),
                        tab_id.clone(),
                        Placement::Step(step as i32),
                    );
                }
            }
            "h" | "left" | "up" | "l" | "right" | "down" => {
                if let Some(index) = index.or_else(|| {
                    workspace
                        .tabs
                        .iter()
                        .position(|tab| tab.tab_id == selected.tab_id)
                }) {
                    let next = if matches!(key, "h" | "left" | "up") {
                        index.saturating_sub(1)
                    } else {
                        (index + 1).min(workspace.tabs.len() - 1)
                    };
                    tab_id = workspace.tabs[next].tab_id.clone();
                }
            }
            _ => {}
        }
        self.navigation = Some(Overlay::Tabs {
            workspace_id,
            tab_id,
        });
    }

    fn close_highlighted(
        &mut self,
        mut target: CloseView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session) = self.session.clone().filter(|session| {
            let ui = session.ui();
            ui.can_close()
                && ui.selected.as_ref().is_some_and(|active| {
                    active.workspace_id == target.workspace_id
                        && (target.pane_id.is_none()
                            || target.tab_id.as_ref() == Some(&active.tab_id))
                })
        }) else {
            return false;
        };
        target.expected_view_revision = Some(session.view_revision());
        self.dismiss_overlay(window, cx);
        session.close(target);
        true
    }

    fn neighbor(&self, view: &ViewSelection, direction: Direction) -> Option<ViewSelection> {
        let (_, inventory) = self.navigation_target()?;
        let tab = inventory
            .workspaces
            .iter()
            .find(|ws| ws.workspace_id == view.workspace_id)?
            .tabs
            .iter()
            .find(|tab| tab.tab_id == view.tab_id)?;
        let mut panes = Vec::new();
        pane_rects(tab, 0, Rect::UNIT, &mut panes);
        let index = Rect::neighbor(&panes, &view.pane_id.as_str(), direction)?;
        Some(ViewSelection {
            pane_id: panes[index].0.into(),
            ..view.clone()
        })
    }

    pub(super) fn navigation_panel(&self) -> Option<Div> {
        let ui = self.session.as_ref().map(Session::ui);
        let can_close = ui.as_ref().is_some_and(Ui::can_close);
        let can_reorder = ui.as_ref().is_some_and(Ui::can_reorder_tabs);
        let mode_hint = match self.navigation.as_ref() {
            Some(Overlay::Tabs { .. }) => Some(tab_mode_hint(can_close, can_reorder).to_owned()),
            Some(Overlay::Panes(target)) => {
                let label = self
                    .pane_ordinal(target)
                    .map_or_else(|| "Panes".into(), |ordinal| format!("Pane {ordinal}"));
                Some(format!("{label} · {}", pane_mode_hint(can_close)))
            }
            _ => None,
        };
        if let Some(hint) = mode_hint {
            return Some(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .px(ui_px(12.))
                    .py(ui_px(5.))
                    .bg(rgb(PANEL))
                    .text_color(rgb(TEXT))
                    .text_size(ui_px(12.))
                    .child(hint),
            );
        }
        let panel = div()
            .absolute()
            .top(ui_px(45.))
            .right(ui_px(8.))
            .w(ui_px(330.))
            .max_w_full()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .p(ui_px(10.))
            .text_color(rgb(TEXT))
            .text_size(ui_px(13.));
        if self.leader {
            return Some(panel.child("w Workspaces · t Tabs · p Panes · ? Search · Esc Cancel"));
        }
        let Some(Overlay::Commands { query, index }) = &self.navigation else {
            return None;
        };
        let index = *index;
        let items = self.commands(query);
        let start = index.saturating_sub(5).min(items.len().saturating_sub(10));
        Some(
            panel.child(format!("Commands · {query}")).children(
                items
                    .into_iter()
                    .enumerate()
                    .skip(start)
                    .take(10)
                    .map(|(row, command)| {
                        div()
                            .px(ui_px(5.))
                            .when(row == index, |row| row.bg(rgb(ACTIVE_FILL)))
                            .child(command.label())
                    }),
            ),
        )
    }

    fn pane_ordinal(&self, target: &ViewSelection) -> Option<usize> {
        let (_, inventory) = self.navigation_target()?;
        let tab = inventory
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == target.workspace_id)?
            .tabs
            .iter()
            .find(|tab| tab.tab_id == target.tab_id)?;
        tab.nodes
            .iter()
            .filter_map(|node| match &node.body {
                Some(Node::PaneId(id)) => Some(id),
                _ => None,
            })
            .position(|id| id == &target.pane_id)
            .map(|index| index + 1)
    }
}

fn matching<'a, T: 'a>(
    items: impl IntoIterator<Item = T>,
    query: &str,
    label: impl Fn(&T) -> &'a str,
) -> Vec<T> {
    let query = query.to_lowercase();
    items
        .into_iter()
        .filter(|item| label(item).to_lowercase().contains(&query))
        .collect()
}

fn workspace_command(key: &gpui::Keystroke) -> Option<WorkspaceCommand> {
    let text = key_text(key)?;
    let name = match (dubeolsik_key(text), key.key.as_bytes()) {
        (Some(_), _) if key.modifiers.shift => return None,
        (Some(_), [b'a'..=b'z']) => key.key.as_str(),
        (Some(mapped), _) => mapped,
        (None, _) => text,
    };
    let [suffix] = name.as_bytes() else {
        return None;
    };
    WorkspaceCommand::from_key(char::from(*suffix))
}

fn key_text(key: &gpui::Keystroke) -> Option<&str> {
    if key.modifiers.control || key.modifiers.platform || key.modifiers.alt {
        return None;
    }
    key.key_char
        .as_deref()
        .or_else(|| (key.key.chars().count() == 1).then_some(key.key.as_str()))
}

fn edit_list(key: &gpui::Keystroke, query: &mut String, index: &mut usize, len: usize) {
    match key.key.as_str() {
        "up" => *index = index.saturating_sub(1),
        "down" => *index = (*index + 1).min(len.saturating_sub(1)),
        "backspace" => {
            query.pop();
            *index = 0;
        }
        _ => {
            if let Some(text) = key_text(key).filter(|text| query.len() + text.len() <= 256) {
                query.push_str(text);
                *index = 0;
            }
        }
    }
}

fn direction(key: &str) -> Option<Direction> {
    Some(match key {
        "left" => Direction::Left,
        "right" => Direction::Right,
        "up" => Direction::Up,
        "down" => Direction::Down,
        _ => return None,
    })
}

fn pane_direction(key: &str) -> Option<Direction> {
    match key {
        "h" => Some(Direction::Left),
        "j" => Some(Direction::Down),
        "k" => Some(Direction::Up),
        "l" => Some(Direction::Right),
        _ => direction(key),
    }
}

fn pane_rects<'a>(tab: &'a TabState, index: usize, region: Rect, panes: &mut Vec<(&'a str, Rect)>) {
    match tab.nodes.get(index).and_then(|node| node.body.as_ref()) {
        Some(Node::PaneId(id)) => panes.push((id, region)),
        Some(Node::Split(split)) => {
            let axis = if split.axis == SplitAxis::Vertical as i32 {
                Axis::Vertical
            } else {
                Axis::Horizontal
            };
            for (index, &child) in split.children.iter().enumerate() {
                pane_rects(
                    tab,
                    child as usize,
                    region.split(axis, index, split.children.len()),
                    panes,
                );
            }
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Keystroke;

    #[test]
    fn leader_commands_match_korean_key_positions() {
        for (stroke, command) in [
            ("w->ㅈ", WorkspaceCommand::Workspaces),
            ("ㅅ", WorkspaceCommand::Tabs),
            ("p->ᅦ", WorkspaceCommand::Panes),
            ("k->ㅏ", WorkspaceCommand::PreviousCommand),
            ("c->ㅊ", WorkspaceCommand::CopyLastOutput),
            ("ㅇ", WorkspaceCommand::Detach),
            ("shift-/->?", WorkspaceCommand::Commands),
        ] {
            assert_eq!(
                workspace_command(&Keystroke::parse(stroke).unwrap()),
                Some(command)
            );
        }
        for stroke in ["shift-t->ㅆ", "ctrl-ㅅ", "t->한글"] {
            assert_eq!(workspace_command(&Keystroke::parse(stroke).unwrap()), None);
        }
    }

    #[test]
    fn command_search_keeps_korean_text() {
        let mut query = String::new();
        let mut index = 0;
        for stroke in ["t->ㅅ", "g->한글"] {
            edit_list(
                &Keystroke::parse(stroke).unwrap(),
                &mut query,
                &mut index,
                1,
            );
        }
        assert_eq!(query, "ㅅ한글");
    }
}
