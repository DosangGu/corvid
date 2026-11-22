use super::*;
use corvid_core::layout::Axis;
use corvid_core::navigation::{Direction, Rect, WorkspaceCommand};
use corvid_protocol::wire::SplitPane;

pub(super) enum Overlay {
    Commands {
        query: String,
        index: usize,
    },
    Tabs {
        query: String,
        index: usize,
        control: u8,
    },
    Panes(ViewSelection),
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
                if let Some((selected, inventory)) = self.navigation_target()
                    && let Some(workspace) = inventory
                        .workspaces
                        .iter()
                        .find(|ws| ws.workspace_id == selected.workspace_id)
                {
                    let index = workspace
                        .tabs
                        .iter()
                        .position(|tab| tab.tab_id == selected.tab_id)
                        .unwrap_or(0);
                    self.show_navigation(
                        Overlay::Tabs {
                            query: String::new(),
                            index,
                            control: 0,
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
            CloseWindow => self.windows.close_active(cx),
            Quit => self.windows.quit(cx),
            Settings => self.open_management(management::Page::Connections, window, cx),
            Agents => {
                if let Some(session) = &self.session {
                    session.open_agent_deck(self.agent_deck.clone());
                }
            }
            Reconnect => self.reconnect_ssh(window, cx),
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
        if event.is_held
            && !matches!(
                key.key.as_str(),
                "left" | "right" | "up" | "down" | "backspace"
            )
        {
            return;
        }
        if key.key == "escape" {
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
            } else if let Some(direction) = direction(&key.key) {
                if let Some((view, _)) = self.navigation_target()
                    && let Some(target) = self.neighbor(&view, direction)
                {
                    self.select(target, false, window, cx);
                }
            } else if let Some(command) = key_text(key).and_then(|text| {
                let mut chars = text.chars();
                let suffix = chars.next()?;
                chars
                    .next()
                    .is_none()
                    .then(|| WorkspaceCommand::from_key(suffix))
                    .flatten()
            }) {
                self.run_command(Command::Workspace(command), window, cx);
            }
            cx.notify();
            return;
        }
        let Some(overlay) = self.navigation.take() else {
            return;
        };
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
                mut query,
                mut index,
                mut control,
            } => {
                let Some((selected, inventory)) = self.navigation_target() else {
                    self.dismiss_overlay(window, cx);
                    return;
                };
                let Some(workspace) = inventory
                    .workspaces
                    .iter()
                    .find(|ws| ws.workspace_id == selected.workspace_id)
                else {
                    self.dismiss_overlay(window, cx);
                    return;
                };
                let tabs = matching(workspace.tabs.iter(), &query, |tab| tab.label.as_str());
                match key.key.as_str() {
                    "tab" => control = (control + if key.modifiers.shift { 2 } else { 1 }) % 3,
                    "enter" if control == 1 => {
                        self.dismiss_overlay(window, cx);
                        self.create(Target::TabLabel(String::new()));
                        return;
                    }
                    "enter" if control == 2 => {
                        if let Some(tab) = tabs.get(index) {
                            let count = inventory
                                .terminals
                                .iter()
                                .filter(|term| {
                                    term.location
                                        .as_ref()
                                        .is_some_and(|view| view.tab_id == tab.tab_id)
                                })
                                .count();
                            self.confirm_close(
                                &format!("End tab {:?} and its {count} terminal(s)?", tab.label),
                                CloseView {
                                    workspace_id: selected.workspace_id,
                                    tab_id: Some(tab.tab_id.clone()),
                                    ..Default::default()
                                },
                                window,
                                cx,
                            );
                        }
                    }
                    "enter" => {
                        if let Some((target, open)) = tabs
                            .get(index)
                            .and_then(|tab| self.choose(workspace, tab, &inventory))
                        {
                            self.dismiss_overlay(window, cx);
                            self.open_pane(target, open, window, cx);
                            return;
                        }
                    }
                    _ if control == 0 => edit_list(key, &mut query, &mut index, tabs.len()),
                    _ => {}
                }
                self.navigation = Some(Overlay::Tabs {
                    query,
                    index,
                    control,
                });
            }
            Overlay::Panes(mut target) => {
                match key.key.as_str() {
                    "enter" => {
                        self.dismiss_overlay(window, cx);
                        self.select(target, false, window, cx);
                        return;
                    }
                    "r" | "d" => {
                        self.dismiss_overlay(window, cx);
                        let axis = if key.key == "r" {
                            SplitAxis::Horizontal
                        } else {
                            SplitAxis::Vertical
                        } as i32;
                        if self.selected.as_ref() == Some(&target) {
                            self.create(Target::SplitAxis(axis));
                        } else {
                            self.create(Target::SplitPane(SplitPane {
                                pane_id: target.pane_id,
                                axis,
                            }));
                        }
                        return;
                    }
                    "x" => {
                        let title = self.navigation_target().and_then(|(_, inventory)| {
                            inventory.workspaces.iter().find(|ws| ws.workspace_id == target.workspace_id)?
                                .tabs.iter().find(|tab| tab.tab_id == target.tab_id)?
                                .nodes.iter().find(|node| matches!(&node.body, Some(Node::PaneId(id)) if *id == target.pane_id))
                                .map(|node| node.title.clone()).filter(|title| !title.is_empty())
                        }).unwrap_or_else(|| "selected pane".into());
                        self.dismiss_overlay(window, cx);
                        self.confirm_close(
                            &format!("End {title}?"),
                            CloseView {
                                workspace_id: target.workspace_id,
                                tab_id: Some(target.tab_id),
                                pane_id: Some(target.pane_id),
                                ..Default::default()
                            },
                            window,
                            cx,
                        );
                        return;
                    }
                    key => {
                        if let Some(next) =
                            direction(key).and_then(|direction| self.neighbor(&target, direction))
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
        let mut panel = div()
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
        let (title, query, index, items) =
            match self.navigation.as_ref()? {
                Overlay::Panes(_) => {
                    return Some(panel.child(
                        "←↓↑→ Select · Enter Focus · r Right · d Down · x Close · Esc Cancel",
                    ));
                }
                Overlay::Commands { query, index } => (
                    "Commands",
                    query,
                    *index,
                    self.commands(query)
                        .into_iter()
                        .map(|item| item.label().to_owned())
                        .collect::<Vec<_>>(),
                ),
                Overlay::Tabs {
                    query,
                    index,
                    control,
                } => {
                    let (selected, inventory) = self.navigation_target()?;
                    let workspace = inventory
                        .workspaces
                        .iter()
                        .find(|ws| ws.workspace_id == selected.workspace_id)?;
                    panel = panel.child(
                        div().flex().gap(ui_px(8.)).children(
                            ["Tabs", "New tab…", "Close selected tab…"]
                                .into_iter()
                                .enumerate()
                                .map(|(i, label)| {
                                    div()
                                        .text_color(rgb(if i == usize::from(*control) {
                                            ACTIVE
                                        } else {
                                            MUTED
                                        }))
                                        .child(label)
                                }),
                        ),
                    );
                    (
                        "Choose tab",
                        query,
                        *index,
                        matching(workspace.tabs.iter(), query, |tab| &tab.label)
                            .into_iter()
                            .map(|tab| tab.label.clone())
                            .collect(),
                    )
                }
            };
        let start = index.saturating_sub(5).min(items.len().saturating_sub(10));
        Some(
            panel.child(format!("{title} · {query}")).children(
                items
                    .into_iter()
                    .enumerate()
                    .skip(start)
                    .take(10)
                    .map(|(row, label)| {
                        div()
                            .px(ui_px(5.))
                            .when(row == index, |row| row.bg(rgb(ACTIVE_FILL)))
                            .child(label)
                    }),
            ),
        )
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
