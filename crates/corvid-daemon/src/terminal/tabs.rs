use std::fmt::Write;

use corvid_core::Viewport;
use corvid_core::layout::Workspace;
use uuid::Uuid;

use super::status::clip;

pub(super) struct TabPicker {
    workspace: String,
    tabs: Vec<Choice>,
    active: usize,
    selected: usize,
    pub(super) dirty: bool,
}

struct Choice {
    id: Uuid,
    label: String,
    live: bool,
}

pub(super) enum PickerAction {
    Keep,
    Cancel,
    Select(Uuid),
}

impl TabPicker {
    pub(super) fn new(
        workspace: &Workspace,
        current: Uuid,
        is_live: impl Fn(Uuid) -> bool,
    ) -> Option<Self> {
        let active = workspace.tabs.iter().position(|tab| tab.id == current)?;
        let tabs = workspace
            .tabs
            .iter()
            .map(|tab| Choice {
                id: tab.id,
                label: tab.label.clone(),
                live: is_live(tab.id),
            })
            .collect();
        Some(Self {
            workspace: workspace.label.clone(),
            tabs,
            active,
            selected: active,
            dirty: true,
        })
    }

    pub(super) fn key(&mut self, bytes: &[u8]) -> PickerAction {
        let selected = match bytes {
            b"\x1b[A" | b"\x1bOA" => self.selected.saturating_sub(1),
            b"\x1b[B" | b"\x1bOB" => (self.selected + 1).min(self.tabs.len() - 1),
            b"\r" | b"\n" if self.tabs[self.selected].live => {
                return PickerAction::Select(self.tabs[self.selected].id);
            }
            b"\x1b" | b"\x03" => return PickerAction::Cancel,
            _ => return PickerAction::Keep,
        };
        self.dirty |= self.selected != selected;
        self.selected = selected;
        PickerAction::Keep
    }

    pub(super) fn paint(&mut self, outer: Viewport) -> Result<String, std::fmt::Error> {
        let cols = usize::from(outer.cols.get());
        let rows = usize::from(outer.rows.get() - 1);
        let start = self.selected.saturating_sub(rows / 2);
        let start = start.min(self.tabs.len().saturating_sub(rows));
        let end = (start + rows).min(self.tabs.len());

        let mut frame = String::from("\x1b[?6l\x1b[r\x1b[?7l\x1b[0m\x1b[?25l\x1b[2J\x1b[H");
        let title = format!(
            "Tabs {}/{} | ↑/↓ Enter Esc | {}",
            self.selected + 1,
            self.tabs.len(),
            self.workspace,
        );
        write!(frame, "\x1b[0;30;46m\x1b[2K{}", clip(&title, cols))?;

        for index in start..end {
            let choice = &self.tabs[index];
            let cursor = if index == self.selected { '>' } else { ' ' };
            let current = if index == self.active { '*' } else { ' ' };
            let closed = if choice.live { "" } else { " (closed)" };
            let label = format!("{cursor}{current} {}{closed}: {}", index + 1, choice.label);
            let style = if index == self.selected {
                "\x1b[0;1;7m"
            } else {
                "\x1b[0m"
            };
            let row = index - start + 2;
            write!(frame, "\x1b[{row};1H{style}{}", clip(&label, cols))?;
        }
        frame.push_str("\x1b[0m\x1b[?7h");
        self.dirty = false;
        Ok(frame)
    }
}
