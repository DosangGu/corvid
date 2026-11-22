use std::fmt::Write;

use corvid_core::Viewport;
use corvid_core::layout::{Axis, CloseTarget, Layout, Location, Node};
use corvid_core::navigation::{Direction, Rect, WorkspaceCommand};
use corvid_protocol::wire::WheelEncoding;
use uuid::Uuid;

use super::keys::{KeyCommand, Keymap, TabCommand};
use super::status::clip;

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Workspaces,
    Tabs,
    Panes,
    Commands,
}

pub(super) enum Overlay {
    List {
        kind: Kind,
        index: usize,
        control: u8,
        query: String,
    },
    Panes(Uuid),
    Confirm {
        title: String,
        target: CloseTarget,
        return_to: Option<Box<Overlay>>,
    },
}

#[derive(Clone, Copy)]
enum Choice {
    Workspace(Uuid),
    Tab(Uuid, usize),
    Command(u8),
    EndWorkspace,
}

impl Overlay {
    pub(super) fn open(kind: Kind, layout: &Layout, location: Location) -> Self {
        if matches!(kind, Kind::Panes) {
            return Self::Panes(location.pane);
        }
        let index = match kind {
            Kind::Workspaces => layout
                .workspaces
                .iter()
                .position(|ws| ws.id == location.workspace),
            Kind::Tabs => layout
                .workspaces
                .iter()
                .find(|ws| ws.id == location.workspace)
                .and_then(|ws| ws.tabs.iter().position(|tab| tab.id == location.tab)),
            _ => None,
        }
        .unwrap_or(0);
        Self::List {
            kind,
            index,
            control: 0,
            query: String::new(),
        }
    }

    pub(super) fn key(
        self,
        bytes: &[u8],
        layout: &Layout,
        location: Location,
        keymap: Keymap,
    ) -> (Option<Self>, Option<KeyCommand>) {
        if matches!(bytes, b"\x1b" | b"\x03") {
            return match self {
                Self::Confirm { return_to, .. } => (return_to.map(|overlay| *overlay), None),
                _ => (None, None),
            };
        }
        let unfocused = |command| (None, Some(command));
        match self {
            Self::Panes(mut pane) => {
                match bytes {
                    b"\r" | b"\n" => return unfocused(KeyCommand::Tab(TabCommand::Pane(pane))),
                    b"r" | b"d" => {
                        let axis = if bytes == b"r" {
                            Axis::Horizontal
                        } else {
                            Axis::Vertical
                        };
                        return unfocused(KeyCommand::Tab(TabCommand::SplitAt(pane, axis)));
                    }
                    b"x" if keymap.close => {
                        let panes = panes(layout, location, Rect::UNIT);
                        let index = panes.iter().position(|(id, _)| *id == pane).unwrap_or(0);
                        return (
                            Some(Self::Confirm {
                                title: format!("End pane {} and its terminal?", index + 1),
                                target: CloseTarget::Pane(Location { pane, ..location }),
                                return_to: Some(Box::new(Self::Panes(pane))),
                            }),
                            None,
                        );
                    }
                    _ => {
                        if let Some(target) =
                            direction(bytes).and_then(|dir| neighbor(layout, location, pane, dir))
                        {
                            pane = target;
                        }
                    }
                }
                (Some(Self::Panes(pane)), None)
            }
            Self::Confirm {
                title,
                target,
                return_to,
            } => {
                if matches!(bytes, b"\r" | b"\n") {
                    (
                        return_to.map(|overlay| *overlay),
                        Some(KeyCommand::End(target)),
                    )
                } else {
                    (
                        Some(Self::Confirm {
                            title,
                            target,
                            return_to,
                        }),
                        None,
                    )
                }
            }
            Self::List {
                kind,
                mut index,
                mut control,
                mut query,
            } => {
                let items = choices(kind, &query, layout, location, keymap);
                index = index.min(items.len().saturating_sub(1));
                match bytes {
                    b"\t" | b"\x1b[Z" if matches!(kind, Kind::Tabs) => {
                        let controls = if keymap.close { 3 } else { 2 };
                        control =
                            (control + if bytes == b"\t" { 1 } else { controls - 1 }) % controls;
                    }
                    b"\r" | b"\n" if control == 1 => {
                        return unfocused(KeyCommand::Tab(TabCommand::Create));
                    }
                    b"\r" | b"\n" => {
                        if let Some((label, choice)) = items.get(index) {
                            match *choice {
                                Choice::Workspace(id) => {
                                    return unfocused(KeyCommand::Tab(TabCommand::Workspace(id)));
                                }
                                Choice::Tab(id, _) if control == 0 => {
                                    return unfocused(KeyCommand::Tab(TabCommand::Select(id)));
                                }
                                Choice::Tab(id, count) => {
                                    return (
                                        Some(Self::Confirm {
                                            title: format!(
                                                "End {label:?} and its {count} pane(s)?"
                                            ),
                                            target: CloseTarget::Tab {
                                                workspace: location.workspace,
                                                tab: id,
                                            },
                                            return_to: Some(Box::new(Self::List {
                                                kind,
                                                index,
                                                control: 0,
                                                query,
                                            })),
                                        }),
                                        None,
                                    );
                                }
                                Choice::Command(key) => return (None, keymap.command(&[key])),
                                Choice::EndWorkspace => {
                                    return (
                                        Some(Self::Confirm {
                                            title: "End the current workspace and its terminals?"
                                                .into(),
                                            target: CloseTarget::Workspace(location.workspace),
                                            return_to: None,
                                        }),
                                        None,
                                    );
                                }
                            }
                        }
                    }
                    _ if control > 0 => {}
                    b"\x1b[A" | b"\x1bOA" => index = index.saturating_sub(1),
                    b"\x1b[B" | b"\x1bOB" => index = (index + 1).min(items.len().saturating_sub(1)),
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
                (
                    Some(Self::List {
                        kind,
                        index,
                        control,
                        query,
                    }),
                    None,
                )
            }
        }
    }

    pub(super) fn paint(
        &self,
        layout: &Layout,
        location: Location,
        keymap: Keymap,
        outer: Viewport,
    ) -> String {
        let mut lines = Vec::new();
        match self {
            Self::List {
                kind,
                index,
                control,
                query,
            } => {
                let (title, hints) = match kind {
                    Kind::Workspaces => ("Workspaces", "↑/↓ Choose · Enter Open · Esc Cancel"),
                    Kind::Tabs => ("Tabs", "↑/↓ Choose · Enter Open · Tab Actions · Esc Cancel"),
                    _ => ("CORVID COMMANDS", "↑/↓ Choose · Enter Run · Esc Cancel"),
                };
                lines.push(format!("{title} · {hints}"));
                lines.push(format!("Search: {query}"));
                if matches!(kind, Kind::Tabs) {
                    lines.push(
                        match control {
                            1 => "Tabs · [New tab…] · Close selected tab…",
                            2 => "Tabs · New tab… · [Close selected tab…]",
                            _ if keymap.close => "[Tabs] · New tab… · Close selected tab…",
                            _ => "[Tabs] · New tab…",
                        }
                        .into(),
                    );
                }
                let items = choices(*kind, query, layout, location, keymap);
                let visible = usize::from(outer.rows.get()).saturating_sub(lines.len() + 1);
                let start = index
                    .saturating_sub(visible / 2)
                    .min(items.len().saturating_sub(visible));
                lines.extend(items.into_iter().enumerate().skip(start).take(visible).map(
                    |(row, (label, _))| {
                        format!("{} {label}", if row == *index { '>' } else { ' ' })
                    },
                ));
            }
            Self::Panes(_) => lines.push(
                if keymap.close {
                    "Panes · ←↓↑→ Select · Enter Focus · r Right · d Down · x Close · Esc Cancel"
                } else {
                    "Panes · ←↓↑→ Select · Enter Focus · r Right · d Down · Esc Cancel"
                }
                .into(),
            ),
            Self::Confirm { title, .. } => {
                lines.push(title.clone());
                lines.push("Enter End terminals · Esc Cancel".into());
            }
        }
        paint_lines(outer, &lines)
    }
}

fn choices(
    kind: Kind,
    query: &str,
    layout: &Layout,
    location: Location,
    keymap: Keymap,
) -> Vec<(String, Choice)> {
    let mut items = match kind {
        Kind::Workspaces => layout
            .workspaces
            .iter()
            .map(|workspace| (workspace.label.clone(), Choice::Workspace(workspace.id)))
            .collect(),
        Kind::Tabs => layout
            .workspaces
            .iter()
            .find(|workspace| workspace.id == location.workspace)
            .map(|ws| {
                ws.tabs
                    .iter()
                    .map(|tab| {
                        let mut panes = Vec::new();
                        tab.root.pane_ids(&mut panes);
                        (tab.label.clone(), Choice::Tab(tab.id, panes.len()))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        Kind::Commands => {
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
        Kind::Panes => Vec::new(),
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
            clip(line, usize::from(outer.cols.get()))
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
