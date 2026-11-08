use corvid_core::Viewport;
use corvid_core::layout::{Layout, Location, Workspace};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::keys::{InputMode, Keymap};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct TuiViewport {
    pub(crate) outer: Viewport,
    pub(crate) status_bar: bool,
}

impl TuiViewport {
    pub(crate) fn content(self) -> Viewport {
        let available = match self.status_row() {
            Some(row) => Viewport::new(self.outer.cols.get(), row - 1)
                .expect("status row leaves at least two content rows"),
            None => self.outer,
        };
        available.fit_screen()
    }

    pub(super) fn status_row(self) -> Option<u16> {
        let rows = self.outer.rows.get();
        (self.status_bar && rows > 2).then_some(rows)
    }
}

pub(super) fn line(
    layout: &Layout,
    location: Location,
    cols: u16,
    mode: InputMode,
    keymap: Keymap,
) -> String {
    let width = usize::from(cols - 1);
    let (style, title, help) = match mode {
        InputMode::Passthrough => {
            let help = if keymap.tabs {
                " | Ctrl-] 1-9 / w tabs"
            } else {
                " | Ctrl-] commands"
            };
            ("\x1b[0;30;46m", "CORVID", help)
        }
        InputMode::Command => {
            let help = if keymap.tabs {
                " | 1-9 switch  w list  t new  d detach"
            } else {
                " | d detach"
            };
            ("\x1b[0;30;43m", "CORVID CMD", help)
        }
    };
    let title = clip(title, width);
    let mut text = format!("{style}\x1b[2K{title}");
    let mut remaining = width - title.len();
    let help = if remaining >= help.len() + 24 {
        help
    } else {
        ""
    };
    remaining -= help.len();

    if let Some(workspace) = layout
        .workspaces
        .iter()
        .find(|item| item.id == location.workspace)
    {
        if remaining >= 26 {
            let name = clip(&workspace.label, (remaining / 4).min(18));
            if !name.is_empty() {
                text.push_str("  ");
                text.push_str(&name);
                remaining -= 2 + name.width();
            }
        }
        append_tabs(&mut text, workspace, location, remaining);
    }
    text.push_str(help);
    text
}

fn append_tabs(text: &mut String, workspace: &Workspace, location: Location, mut room: usize) {
    let Some(active) = workspace.tabs.iter().position(|tab| tab.id == location.tab) else {
        return;
    };
    let tabs: Vec<_> = workspace
        .tabs
        .iter()
        .enumerate()
        .map(|(index, tab)| tab_label(index, &tab.label, index == active, room))
        .collect();
    let mut through_active: usize = tabs[..=active].iter().map(|tab| 1 + tab.width()).sum();
    let mut start = 0;
    while through_active > room && start < active {
        through_active -= 1 + tabs[start].width();
        start += 1;
    }
    if start > 0 && room >= through_active + 2 {
        text.push_str(" <");
        room -= 2;
    }
    for (index, tab) in tabs.iter().enumerate().skip(start) {
        let width = 1 + tab.width();
        if tab.is_empty() || width > room {
            if room >= 2 {
                text.push_str(" >");
            }
            break;
        }
        text.push(' ');
        if index == active {
            text.push_str("\x1b[1;7m");
            text.push_str(tab);
            text.push_str("\x1b[22;27m");
        } else {
            text.push_str(tab);
        }
        room -= width;
    }
}

fn tab_label(index: usize, label: &str, active: bool, room: usize) -> String {
    let number = (index + 1).to_string();
    let fixed = 1 + number.len() + if active { 2 } else { 0 };
    if fixed > room {
        return String::new();
    }
    let name = clip(label, room.saturating_sub(fixed + 1).min(22));
    let item = if name.is_empty() {
        number
    } else {
        format!("{number}:{name}")
    };
    if active { format!("[{item}]") } else { item }
}

pub(super) fn clip(text: &str, mut room: usize) -> String {
    let mut result = String::new();
    for ch in text.chars().filter(|ch| !ch.is_control()) {
        let width = ch.width().unwrap_or(0);
        if width > room {
            break;
        }
        if width > 0 {
            result.push(ch);
            room -= width;
        }
    }
    result
}
