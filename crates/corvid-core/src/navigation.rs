//! Workspace commands and spatial pane navigation shared by the presentations.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

pub const fn tab_mode_hint(can_close: bool, can_reorder: bool) -> &'static str {
    match (can_close, can_reorder) {
        (true, true) => {
            "Tabs · [/] Move · ←/→ h/l Preview · Enter Focus · n New · x Close · Esc Exit"
        }
        (false, true) => "Tabs · [/] Move · ←/→ h/l Preview · Enter Focus · n New · Esc Exit",
        (true, false) => "Tabs · ←/→ h/l Preview · Enter Focus · n New · x Close · Esc Cancel",
        (false, false) => "Tabs · ←/→ h/l Preview · Enter Focus · n New · Esc Cancel",
    }
}

pub const fn pane_mode_hint(can_close: bool) -> &'static str {
    if can_close {
        "←↓↑→ h/j/k/l · Enter Focus · r Right · d Down · x Close · Esc Cancel"
    } else {
        "←↓↑→ h/j/k/l · Enter Focus · r Right · d Down · Esc Cancel"
    }
}

pub const fn agent_launcher_hint(can_split: bool) -> &'static str {
    if can_split {
        "↑/↓ Choose · Enter New tab · r Split right · d Split down · Esc Cancel"
    } else {
        "↑/↓ Choose · Enter New tab · Esc Cancel"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceCommand {
    Workspaces,
    Tabs,
    Panes,
    LaunchAgent,
    PreviousTab,
    NextTab,
    RenameTab,
    Commands,
    Detach,
}

impl WorkspaceCommand {
    pub const ALL: [Self; 9] = [
        Self::Workspaces,
        Self::Tabs,
        Self::Panes,
        Self::LaunchAgent,
        Self::PreviousTab,
        Self::NextTab,
        Self::RenameTab,
        Self::Commands,
        Self::Detach,
    ];

    pub const fn key(self) -> char {
        match self {
            Self::Workspaces => 'w',
            Self::Tabs => 't',
            Self::Panes => 'p',
            Self::LaunchAgent => 'a',
            Self::PreviousTab => '[',
            Self::NextTab => ']',
            Self::RenameTab => '.',
            Self::Commands => '?',
            Self::Detach => 'd',
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Workspaces => "Switch workspace",
            Self::Tabs => "Tabs",
            Self::Panes => "Panes",
            Self::LaunchAgent => "Launch agent",
            Self::PreviousTab => "Previous tab",
            Self::NextTab => "Next tab",
            Self::RenameTab => "Rename tab",
            Self::Commands => "Search commands",
            Self::Detach => "Detach (keep terminals running)",
        }
    }

    pub fn from_key(key: char) -> Option<Self> {
        Self::ALL.into_iter().find(|command| command.key() == key)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub const UNIT: Self = Self {
        x: 0,
        y: 0,
        width: 32768,
        height: 32768,
    };

    pub fn split(self, axis: crate::layout::Axis, index: usize, len: usize) -> Self {
        let range = |size: u16| {
            let boundary = |index| (usize::from(size) * index / len) as u16;
            let start = boundary(index);
            (start, boundary(index + 1) - start)
        };
        match axis {
            crate::layout::Axis::Horizontal => {
                let (offset, width) = range(self.width);
                Self {
                    x: self.x + offset,
                    width,
                    ..self
                }
            }
            crate::layout::Axis::Vertical => {
                let (offset, height) = range(self.height);
                Self {
                    y: self.y + offset,
                    height,
                    ..self
                }
            }
        }
    }

    pub fn contains(self, x: u16, y: u16) -> bool {
        x >= self.x && x - self.x < self.width && y >= self.y && y - self.y < self.height
    }

    fn coordinates(self, direction: Direction) -> (i32, i32, i32, i32) {
        let x = i32::from(self.x);
        let y = i32::from(self.y);
        let right = x + i32::from(self.width);
        let bottom = y + i32::from(self.height);
        match direction {
            Direction::Right => (x, right, y, bottom),
            Direction::Left => (-right, -x, y, bottom),
            Direction::Down => (y, bottom, x, right),
            Direction::Up => (-bottom, -y, x, right),
        }
    }

    pub fn neighbor<T: Eq>(
        panes: &[(T, Self)],
        current: &T,
        direction: Direction,
    ) -> Option<usize> {
        let (_, origin) = panes.iter().find(|(pane, _)| pane == current)?;
        let (_, edge, top, bottom) = origin.coordinates(direction);
        panes
            .iter()
            .enumerate()
            .filter_map(|(index, (_, candidate))| {
                let (near, _, start, end) = candidate.coordinates(direction);
                let overlap = bottom.min(end) - top.max(start);
                (near >= edge && overlap > 0)
                    .then_some(((near - edge, (top + bottom).abs_diff(start + end)), index))
            })
            .min()
            .map(|(_, index)| index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Axis;

    #[test]
    fn picks_adjacent_panes_in_a_nested_layout() {
        let left = Rect::UNIT.split(Axis::Horizontal, 0, 2);
        let right = Rect::UNIT.split(Axis::Horizontal, 1, 2);
        let panes = [
            ("left", left),
            ("top", right.split(Axis::Vertical, 0, 2)),
            ("bottom", right.split(Axis::Vertical, 1, 2)),
        ];
        assert_eq!(Rect::neighbor(&panes, &"left", Direction::Right), Some(1));
        assert_eq!(Rect::neighbor(&panes, &"top", Direction::Down), Some(2));
        assert_eq!(Rect::neighbor(&panes, &"bottom", Direction::Left), Some(0));
        assert_eq!(Rect::neighbor(&panes, &"bottom", Direction::Down), None);
    }

    #[test]
    fn launch_agent_command_uses_a_key_and_label() {
        assert_eq!(WorkspaceCommand::LaunchAgent.key(), 'a');
        assert_eq!(WorkspaceCommand::LaunchAgent.label(), "Launch agent");
        assert_eq!(
            WorkspaceCommand::from_key('a'),
            Some(WorkspaceCommand::LaunchAgent)
        );
        assert!(WorkspaceCommand::ALL.contains(&WorkspaceCommand::LaunchAgent));
    }
}
