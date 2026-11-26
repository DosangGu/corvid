use std::fmt::Write;
use std::sync::Arc;

use corvid_core::Viewport;
use corvid_core::agent::Provider;
use corvid_core::layout::{Axis, CloseTarget, Layout, Location, Node, TabPlacement};
use corvid_core::navigation::{
    Direction, Rect, WorkspaceCommand, agent_launcher_hint, pane_mode_hint, tab_mode_hint,
};
use corvid_protocol::wire::WheelEncoding;
use uuid::Uuid;

use super::keys::{KeyCommand, Keymap, TabCommand, shortcut_keys};
use super::title_motion::{TitleMotion, TitleTarget, window};

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Workspaces,
    Tabs,
    Panes,
    Agents,
    Commands,
}

pub(super) enum Overlay {
    List {
        kind: ListKind,
        index: usize,
        query: String,
        selected: Option<Uuid>,
    },
    Tabs(Uuid),
    Panes(Uuid),
    Agents {
        index: usize,
    },
    ConfirmWorkspace(Uuid),
}

#[derive(Clone, Copy)]
pub(super) enum ListKind {
    Workspaces,
    Commands,
}

#[derive(Clone, Copy)]
enum Choice {
    Workspace(Uuid),
    Command(u8),
    PreviousCommand,
    CopyLastOutput,
    EndWorkspace,
}

impl Choice {
    fn id(self) -> Option<Uuid> {
        match self {
            Self::Workspace(id) => Some(id),
            _ => None,
        }
    }
}

impl Overlay {
    pub(super) fn open(kind: Kind, layout: &Layout, location: Location) -> Self {
        let (kind, index, selected) = match kind {
            Kind::Tabs => return Self::Tabs(location.tab),
            Kind::Panes => return Self::Panes(location.pane),
            Kind::Agents => return Self::Agents { index: 0 },
            Kind::Commands => (ListKind::Commands, 0, None),
            Kind::Workspaces => (
                ListKind::Workspaces,
                layout
                    .workspaces
                    .iter()
                    .position(|ws| ws.id == location.workspace)
                    .unwrap_or(0),
                Some(location.workspace),
            ),
        };
        Self::List {
            kind,
            index,
            query: String::new(),
            selected,
        }
    }

    pub(super) fn key(
        self,
        bytes: &[u8],
        layout: &Layout,
        location: Location,
        keymap: Keymap,
        reorder: impl FnMut(Uuid, TabPlacement) -> Option<Arc<Layout>>,
    ) -> (Option<Self>, Option<KeyCommand>) {
        if matches!(bytes, b"\x1b" | b"\x03") {
            return (None, None);
        }
        let unfocused = |command| (None, Some(command));
        match self {
            Self::Tabs(tab) => {
                tab_keys(shortcut_keys(bytes), layout, location, keymap, tab, reorder)
            }
            Self::Panes(pane) => pane_keys(shortcut_keys(bytes), layout, location, keymap, pane),
            Self::Agents { index } => agent_keys(bytes, keymap, index),
            Self::ConfirmWorkspace(workspace) if matches!(bytes, b"\r" | b"\n") => {
                unfocused(KeyCommand::End(CloseTarget::Workspace(workspace)))
            }
            Self::ConfirmWorkspace(_) => (Some(self), None),
            Self::List {
                kind,
                mut index,
                mut query,
                mut selected,
            } => {
                let items = choices(kind, &query, layout, keymap);
                index = selected
                    .and_then(|id| items.iter().position(|(_, choice)| choice.id() == Some(id)))
                    .unwrap_or_else(|| index.min(items.len().saturating_sub(1)));
                let original_query = query.clone();
                let mut moved = false;
                match bytes {
                    b"\r" | b"\n" => {
                        if let Some((_, choice)) = items.get(index)
                            && (choice.id().is_none() || choice.id() == selected)
                        {
                            match *choice {
                                Choice::Workspace(id) => {
                                    return unfocused(KeyCommand::Tab(TabCommand::Workspace(id)));
                                }
                                Choice::Command(key) => return (None, keymap.command(&[key])),
                                Choice::PreviousCommand => {
                                    return (None, Some(KeyCommand::PreviousCommand));
                                }
                                Choice::CopyLastOutput => {
                                    return (None, Some(KeyCommand::CopyLastOutput));
                                }
                                Choice::EndWorkspace => {
                                    return (
                                        Some(Self::ConfirmWorkspace(location.workspace)),
                                        None,
                                    );
                                }
                            }
                        }
                    }
                    b"\x1b[A" | b"\x1bOA" => {
                        index = index.saturating_sub(1);
                        moved = true;
                    }
                    b"\x1b[B" | b"\x1bOB" => {
                        index = (index + 1).min(items.len().saturating_sub(1));
                        moved = true;
                    }
                    b"\x7f" | b"\x08" => {
                        query.pop();
                        index = 0;
                    }
                    _ => {
                        if let Ok(text) = std::str::from_utf8(bytes)
                            && !text.is_empty()
                            && text.chars().all(|ch| !ch.is_control())
                            && query.len() + text.len() <= 256
                        {
                            query.push_str(text);
                            index = 0;
                        }
                    }
                }
                if original_query != query {
                    selected = choices(kind, &query, layout, keymap)
                        .first()
                        .and_then(|(_, choice)| choice.id());
                } else if moved {
                    selected = items.get(index).and_then(|(_, choice)| choice.id());
                }
                (
                    Some(Self::List {
                        kind,
                        index,
                        query,
                        selected,
                    }),
                    None,
                )
            }
        }
    }

    pub(super) fn title_target<'a>(
        &self,
        layout: &'a Layout,
        outer: Viewport,
    ) -> Option<TitleTarget<'a>> {
        let Self::List {
            kind: ListKind::Workspaces,
            selected: Some(id),
            query,
            ..
        } = self
        else {
            return None;
        };
        // Keep a terminal row above the workspace picker and its controls.
        if outer.rows.get() <= 3 {
            return None;
        }
        let title = &layout
            .workspaces
            .iter()
            .find(|workspace| workspace.id == *id)?
            .label;
        title
            .to_lowercase()
            .contains(&query.to_lowercase())
            .then_some((
                *id,
                title.as_str(),
                usize::from(outer.cols.get().saturating_sub(2)),
            ))
    }

    pub(super) fn paint(
        &self,
        layout: &Layout,
        location: Location,
        keymap: Keymap,
        outer: Viewport,
        motion: &TitleMotion,
    ) -> String {
        let mut lines = Vec::new();
        match self {
            Self::List {
                kind,
                index,
                query,
                selected,
            } => {
                let (title, hints) = match kind {
                    ListKind::Workspaces => ("Workspaces", "↑/↓ Choose · Enter Open · Esc Cancel"),
                    ListKind::Commands => {
                        ("CORVID COMMANDS", "↑/↓ Choose · Enter Run · Esc Cancel")
                    }
                };
                lines.push(format!("{title} · {hints}"));
                lines.push(format!("Search: {query}"));
                let items = choices(*kind, query, layout, keymap);
                let index = selected
                    .and_then(|id| items.iter().position(|(_, choice)| choice.id() == Some(id)))
                    .unwrap_or(*index);
                let visible = usize::from(outer.rows.get()).saturating_sub(lines.len() + 1);
                let start = index
                    .saturating_sub(visible / 2)
                    .min(items.len().saturating_sub(visible));
                lines.extend(items.into_iter().enumerate().skip(start).take(visible).map(
                    |(row, (label, choice))| {
                        if row == index && choice.id().is_some() && choice.id() == *selected {
                            format!("> {}", motion.visible())
                        } else {
                            format!("{} {label}", if row == index { '>' } else { ' ' })
                        }
                    },
                ));
            }
            Self::Tabs(_) => lines.push(tab_mode_hint(keymap.close, true).into()),
            Self::Panes(pane) => {
                let label = layout
                    .panes(location.workspace, location.tab)
                    .iter()
                    .position(|item| item.pane == *pane)
                    .map_or_else(|| "Panes".into(), |index| format!("Pane {}", index + 1));
                lines.push(format!("{label} · {}", pane_mode_hint(keymap.close)));
            }
            Self::Agents { index } => {
                lines.push(format!(
                    "Launch agent · {}",
                    agent_launcher_hint(keymap.panes)
                ));
                lines.extend(
                    Provider::LAUNCHABLE
                        .iter()
                        .enumerate()
                        .map(|(row, provider)| {
                            format!(
                                "{} {}",
                                if row == *index { '>' } else { ' ' },
                                provider.label()
                            )
                        }),
                );
            }
            Self::ConfirmWorkspace(_) => {
                lines.push("End the current workspace and its terminals?".into());
                lines.push("Enter End terminals · Esc Cancel".into());
            }
        }
        paint_lines(outer, &lines)
    }
}

fn agent_keys(
    bytes: &[u8],
    keymap: Keymap,
    mut index: usize,
) -> (Option<Overlay>, Option<KeyCommand>) {
    let count = Provider::LAUNCHABLE.len();
    if count == 0 {
        return (None, None);
    }
    index = index.min(count - 1);
    let provider = Provider::LAUNCHABLE[index];
    if matches!(bytes, b"\r" | b"\n") {
        return (
            None,
            Some(KeyCommand::Tab(TabCommand::LaunchAgent(provider, None))),
        );
    }
    match bytes {
        b"\x1b[A" | b"\x1bOA" => index = index.saturating_sub(1),
        b"\x1b[B" | b"\x1bOB" => index = (index + 1).min(count - 1),
        _ => {
            for key in shortcut_keys(bytes) {
                match key {
                    b'k' => index = index.saturating_sub(1),
                    b'j' => index = (index + 1).min(count - 1),
                    b'r' if keymap.panes => {
                        return (
                            None,
                            Some(KeyCommand::Tab(TabCommand::LaunchAgent(
                                provider,
                                Some(Axis::Horizontal),
                            ))),
                        );
                    }
                    b'd' if keymap.panes => {
                        return (
                            None,
                            Some(KeyCommand::Tab(TabCommand::LaunchAgent(
                                provider,
                                Some(Axis::Vertical),
                            ))),
                        );
                    }
                    _ => {}
                }
            }
        }
    }
    (Some(Overlay::Agents { index }), None)
}

fn pane_keys(
    bytes: &[u8],
    layout: &Layout,
    location: Location,
    keymap: Keymap,
    mut pane: Uuid,
) -> (Option<Overlay>, Option<KeyCommand>) {
    if matches!(bytes, b"\r" | b"\n") {
        return (None, Some(KeyCommand::Tab(TabCommand::Pane(pane))));
    }
    let step = |pane, dir| neighbor(layout, location, pane, dir).unwrap_or(pane);
    if let Some(dir) = direction(bytes) {
        return (Some(Overlay::Panes(step(pane, dir))), None);
    }
    if bytes.iter().all(u8::is_ascii_graphic) {
        // Several printable keys can arrive in the same input chunk.
        for key in bytes {
            match key {
                b'h' => pane = step(pane, Direction::Left),
                b'j' => pane = step(pane, Direction::Down),
                b'k' => pane = step(pane, Direction::Up),
                b'l' => pane = step(pane, Direction::Right),
                b'r' | b'd' => {
                    let axis = if *key == b'r' {
                        Axis::Horizontal
                    } else {
                        Axis::Vertical
                    };
                    return (None, Some(KeyCommand::Tab(TabCommand::SplitAt(pane, axis))));
                }
                b'x' if keymap.close => {
                    return (
                        None,
                        Some(KeyCommand::End(CloseTarget::Pane(Location {
                            pane,
                            ..location
                        }))),
                    );
                }
                _ => {}
            }
        }
    }
    (Some(Overlay::Panes(pane)), None)
}

fn tab_keys(
    bytes: &[u8],
    layout: &Layout,
    location: Location,
    keymap: Keymap,
    mut tab: Uuid,
    mut reorder: impl FnMut(Uuid, TabPlacement) -> Option<Arc<Layout>>,
) -> (Option<Overlay>, Option<KeyCommand>) {
    if matches!(bytes, b"\r" | b"\n") {
        return (None, Some(KeyCommand::Tab(TabCommand::Select(tab))));
    }
    let keys = match direction(bytes) {
        Some(Direction::Left | Direction::Up) => b"h",
        Some(Direction::Right | Direction::Down) => b"l",
        None if bytes.iter().all(u8::is_ascii_graphic) => bytes,
        None => return (Some(Overlay::Tabs(tab)), None),
    };
    // The client may send consecutive printable keys in one input chunk.
    let mut updated = None;
    for key in keys {
        match key {
            b'h' | b'l' => {
                tab = step_tab(
                    updated.as_deref().unwrap_or(layout),
                    location,
                    tab,
                    *key == b'h',
                );
            }
            b'[' | b']' => {
                let step = if *key == b'[' {
                    TabPlacement::Left
                } else {
                    TabPlacement::Right
                };
                let Some(layout) = reorder(tab, step) else {
                    break;
                };
                updated = Some(layout);
            }
            b'n' => return (None, Some(KeyCommand::Tab(TabCommand::Create))),
            b'x' if keymap.close => {
                return (
                    None,
                    Some(KeyCommand::End(CloseTarget::Tab {
                        workspace: location.workspace,
                        tab,
                    })),
                );
            }
            _ => {}
        }
    }
    (Some(Overlay::Tabs(tab)), None)
}

fn step_tab(layout: &Layout, location: Location, selected: Uuid, previous: bool) -> Uuid {
    let Some(workspace) = layout
        .workspaces
        .iter()
        .find(|ws| ws.id == location.workspace)
    else {
        return selected;
    };
    let Some(index) = workspace
        .tabs
        .iter()
        .position(|tab| tab.id == selected)
        .or_else(|| workspace.tabs.iter().position(|tab| tab.id == location.tab))
    else {
        return selected;
    };
    let next = if previous {
        index.saturating_sub(1)
    } else {
        (index + 1).min(workspace.tabs.len() - 1)
    };
    workspace.tabs[next].id
}

fn choices(kind: ListKind, query: &str, layout: &Layout, keymap: Keymap) -> Vec<(String, Choice)> {
    let mut items = match kind {
        ListKind::Workspaces => layout
            .workspaces
            .iter()
            .map(|workspace| (workspace.label.clone(), Choice::Workspace(workspace.id)))
            .collect(),
        ListKind::Commands => {
            let mut commands: Vec<_> = WorkspaceCommand::ALL
                .into_iter()
                .filter_map(|action| {
                    let key = action.key() as u8;
                    keymap.command(&[key]).map(|_| {
                        (
                            format!("{}  {}", action.key(), action.label()),
                            Choice::Command(key),
                        )
                    })
                })
                .collect();
            if keymap.panes {
                commands.extend([
                    ("Split right".into(), Choice::Command(b'%')),
                    ("Split down".into(), Choice::Command(b'"')),
                ]);
            }
            if keymap.command_history {
                commands.push(("Previous command".into(), Choice::PreviousCommand));
                commands.push(("Copy last command output".into(), Choice::CopyLastOutput));
            }
            if keymap.close {
                commands.push(("End workspace…".into(), Choice::EndWorkspace));
            }
            if keymap.pairing {
                commands.push(("Pair external controller".into(), Choice::Command(b'r')));
            }
            if keymap.disconnect {
                commands.push(("Disconnect other clients".into(), Choice::Command(b'D')));
            }
            commands
        }
    };
    let query = query.to_lowercase();
    items.retain(|(label, _)| label.to_lowercase().contains(&query));
    items
}

pub(super) fn paint_lines(outer: Viewport, lines: &[String]) -> String {
    let rows = usize::from(outer.rows.get());
    let count = lines.len().min(rows);
    let mut frame = String::from("\x1b[?25l\x1b[?7l\x1b[0m");
    for (index, line) in lines.iter().take(count).enumerate() {
        let row = rows - count + index + 1;
        let style = if index == 0 { "\x1b[0;7m" } else { "\x1b[0m" };
        let _ = write!(
            frame,
            "\x1b[{row};1H{style}\x1b[2K{}",
            window(line, usize::from(outer.cols.get()), 0)
        );
    }
    frame.push_str("\x1b[0m\x1b[?7h");
    frame
}

pub(super) fn panes(layout: &Layout, location: Location, rect: Rect) -> Vec<(Uuid, Rect)> {
    let Some(tab) = layout
        .workspaces
        .iter()
        .find(|ws| ws.id == location.workspace)
        .and_then(|ws| ws.tabs.iter().find(|tab| tab.id == location.tab))
    else {
        return Vec::new();
    };
    fn visit(node: &Node, rect: Rect, panes: &mut Vec<(Uuid, Rect)>) {
        match node {
            Node::Pane { id, .. } => panes.push((*id, rect)),
            Node::Split { axis, children } => {
                for (index, child) in children.iter().enumerate() {
                    visit(child, rect.split(*axis, index, children.len()), panes);
                }
            }
        }
    }
    let mut panes = Vec::new();
    visit(&tab.root, rect, &mut panes);
    panes
}

pub(super) fn neighbor(
    layout: &Layout,
    location: Location,
    pane: Uuid,
    direction: Direction,
) -> Option<Uuid> {
    let panes = panes(layout, location, Rect::UNIT);
    Rect::neighbor(&panes, &pane, direction).map(|index| panes[index].0)
}

pub(super) fn direction(bytes: &[u8]) -> Option<Direction> {
    Some(match bytes {
        b"\x1b[D" | b"\x1bOD" => Direction::Left,
        b"\x1b[C" | b"\x1bOC" => Direction::Right,
        b"\x1b[A" | b"\x1bOA" => Direction::Up,
        b"\x1b[B" | b"\x1bOB" => Direction::Down,
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Mouse {
    pub(super) x: u16,
    pub(super) y: u16,
    button: u8,
    press: bool,
}

impl Mouse {
    pub(super) fn parse(bytes: &[u8]) -> Option<Self> {
        let params = std::str::from_utf8(bytes.strip_prefix(b"\x1b[<")?).ok()?;
        let press = params.ends_with('M');
        let mut fields = params.strip_suffix(['M', 'm'])?.split(';');
        let button = fields.next()?.parse().ok()?;
        let x = fields.next()?.parse::<u16>().ok()?.checked_sub(1)?;
        let y = fields.next()?.parse::<u16>().ok()?.checked_sub(1)?;
        fields.next().is_none().then_some(Self {
            x,
            y,
            button,
            press,
        })
    }

    pub(super) fn is_primary(self) -> bool {
        self.button == 0 && self.press
    }

    pub(super) fn relative(self, rect: Rect) -> Option<Self> {
        rect.contains(self.x, self.y).then(|| Self {
            x: self.x - rect.x,
            y: self.y - rect.y,
            ..self
        })
    }

    pub(super) fn encode(self, encoding: WheelEncoding, complete_click: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        let press = if complete_click {
            &[true, false][..]
        } else {
            std::slice::from_ref(&self.press)
        };
        for &press in press {
            match encoding {
                WheelEncoding::Sgr => {
                    let end = if press { 'M' } else { 'm' };
                    bytes.extend(
                        format!("\x1b[<{};{};{}{end}", self.button, self.x + 1, self.y + 1).bytes(),
                    );
                }
                WheelEncoding::Legacy if self.x < 223 && self.y < 223 => {
                    let button = if press {
                        self.button
                    } else {
                        (self.button & !3) | 3
                    };
                    bytes.extend([
                        27,
                        b'[',
                        b'M',
                        button.saturating_add(32),
                        self.x as u8 + 33,
                        self.y as u8 + 33,
                    ]);
                }
                WheelEncoding::Utf8 if self.x < 2015 && self.y < 2015 => {
                    let button = if press {
                        self.button
                    } else {
                        (self.button & !3) | 3
                    };
                    bytes.extend(b"\x1b[M");
                    for code in [u16::from(button) + 32, self.x + 33, self.y + 33] {
                        let mut buffer = [0; 4];
                        bytes.extend(
                            char::from_u32(u32::from(code))
                                .unwrap_or(' ')
                                .encode_utf8(&mut buffer)
                                .bytes(),
                        );
                    }
                }
                _ => {}
            }
        }
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_layout() -> (Layout, Location) {
        let mut layout = Layout::empty_terminal();
        let location = layout.add_workspace("main".into(), "Shell".into()).unwrap();
        (layout, location)
    }

    fn workspace_keymap() -> Keymap {
        Keymap {
            workspace_ui: true,
            tabs: true,
            panes: true,
            ..Default::default()
        }
    }

    #[test]
    fn keymap_opens_agent_launcher_for_ascii_and_dubeolsik() {
        let keymap = workspace_keymap();
        assert!(matches!(
            keymap.command(b"a"),
            Some(KeyCommand::Browse(Kind::Agents))
        ));
        assert!(matches!(
            keymap.command("ㅁ".as_bytes()),
            Some(KeyCommand::Browse(Kind::Agents))
        ));
    }

    #[test]
    fn agent_launcher_overlay_navigates_and_launches() {
        let (layout, location) = test_layout();
        let keymap = workspace_keymap();
        let reorder = |_, _| None;

        let overlay = Overlay::open(Kind::Agents, &layout, location);
        let (next, cmd) = overlay.key(b"\x1b[B", &layout, location, keymap, reorder);
        assert!(cmd.is_none());
        let overlay = next.expect("overlay remains open");

        let (next, cmd) = overlay.key(b"\r", &layout, location, keymap, reorder);
        assert!(next.is_none());
        assert!(matches!(
            cmd,
            Some(KeyCommand::Tab(TabCommand::LaunchAgent(
                Provider::Codex,
                None
            )))
        ));

        let overlay = Overlay::open(Kind::Agents, &layout, location);
        let (next, cmd) = overlay.key(b"r", &layout, location, keymap, reorder);
        assert!(next.is_none());
        assert!(matches!(
            cmd,
            Some(KeyCommand::Tab(TabCommand::LaunchAgent(
                Provider::Claude,
                Some(Axis::Horizontal)
            )))
        ));

        let overlay = Overlay::open(Kind::Agents, &layout, location);
        let (next, cmd) = overlay.key(b"d", &layout, location, keymap, reorder);
        assert!(next.is_none());
        assert!(matches!(
            cmd,
            Some(KeyCommand::Tab(TabCommand::LaunchAgent(
                Provider::Claude,
                Some(Axis::Vertical)
            )))
        ));

        let overlay = Overlay::open(Kind::Agents, &layout, location);
        let (next, cmd) = overlay.key(b"\x1b", &layout, location, keymap, reorder);
        assert!(next.is_none());
        assert!(cmd.is_none());
    }
}
