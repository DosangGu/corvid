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
    query: String,
    pub(super) dirty: bool,
}

struct Choice {
    id: Uuid,
    label: String,
    live: bool,
    panes: usize,
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
            .map(|tab| {
                let mut panes = Vec::new();
                tab.root.pane_ids(&mut panes);
                Choice {
                    id: tab.id,
                    label: tab.label.clone(),
                    live: is_live(tab.id),
                    panes: panes.len(),
                }
            })
            .collect();
        Some(Self {
            workspace: workspace.label.clone(),
            tabs,
            active,
            selected: active,
            query: String::new(),
            dirty: true,
        })
    }

    pub(super) fn refresh_names(&mut self, workspace: &Workspace) {
        for choice in &mut self.tabs {
            if let Some(tab) = workspace.tabs.iter().find(|tab| tab.id == choice.id)
                && choice.label != tab.label
            {
                choice.label.clone_from(&tab.label);
                self.dirty = true;
            }
        }
        self.selected = self.selected.min(self.filtered().len().saturating_sub(1));
    }

    pub(super) fn key(&mut self, bytes: &[u8]) -> PickerAction {
        let filtered = self.filtered();
        let selected = match bytes {
            b"\x1b[A" | b"\x1bOA" => self.selected.saturating_sub(1),
            b"\x1b[B" | b"\x1bOB" => (self.selected + 1).min(filtered.len().saturating_sub(1)),
            b"\r" | b"\n" => {
                return filtered
                    .get(self.selected)
                    .map_or(PickerAction::Keep, |&index| {
                        if self.tabs[index].live {
                            PickerAction::Select(self.tabs[index].id)
                        } else {
                            PickerAction::Keep
                        }
                    });
            }
            b"\x1b" | b"\x03" => return PickerAction::Cancel,
            b"\x7f" | b"\x08" => {
                self.query.pop();
                self.dirty = true;
                0
            }
            _ => {
                if let Ok(text) = std::str::from_utf8(bytes)
                    && !text.is_empty()
                    && text.chars().all(|ch| !ch.is_control())
                    && self.query.len() + text.len() <= 256
                {
                    self.query.push_str(text);
                    self.dirty = true;
                    self.selected = 0;
                }
                return PickerAction::Keep;
            }
        };
        self.dirty |= self.selected != selected;
        self.selected = selected;
        PickerAction::Keep
    }

    pub(super) fn paint(&mut self, outer: Viewport) -> Result<String, std::fmt::Error> {
        let cols = usize::from(outer.cols.get());
        let rows = usize::from(outer.rows.get().saturating_sub(2));
        let filtered = self.filtered();
        let start = self.selected.saturating_sub(rows / 2);
        let start = start.min(filtered.len().saturating_sub(rows));
        let end = (start + rows).min(filtered.len());

        let mut frame = String::from("\x1b[?6l\x1b[r\x1b[?7l\x1b[0m\x1b[?25l\x1b[2J\x1b[H");
        let title = format!(
            "Tabs {}/{} | ↑/↓ Enter Esc | {}",
            if filtered.is_empty() {
                0
            } else {
                self.selected + 1
            },
            filtered.len(),
            self.workspace,
        );
        write!(frame, "\x1b[0;7m\x1b[2K{}", clip(&title, cols))?;
        let search = if self.query.is_empty() {
            "Search: type a tab name".to_owned()
        } else {
            format!("Search: {}", self.query)
        };
        if outer.rows.get() > 1 {
            write!(frame, "\x1b[2;1H\x1b[0m{}", clip(&search, cols))?;
        }

        for (position, &index) in filtered.iter().enumerate().take(end).skip(start) {
            let choice = &self.tabs[index];
            let cursor = if position == self.selected { '>' } else { ' ' };
            let current = if index == self.active { '*' } else { ' ' };
            let closed = if choice.live { "" } else { " (closed)" };
            let label = format!(
                "{cursor}{current} {}{closed}: {} · {} pane(s)",
                index + 1,
                choice.label,
                choice.panes
            );
            let style = if position == self.selected {
                "\x1b[0;1;7m"
            } else {
                "\x1b[0m"
            };
            let row = position - start + 3;
            write!(frame, "\x1b[{row};1H{style}{}", clip(&label, cols))?;
        }
        frame.push_str("\x1b[0m\x1b[?7h");
        self.dirty = false;
        Ok(frame)
    }

    fn filtered(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        self.tabs
            .iter()
            .enumerate()
            .filter(|(_, choice)| choice.label.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use corvid_core::layout::Layout;

    #[test]
    fn search_keeps_exact_tab_identity_and_handles_empty_results() {
        let mut layout = Layout::empty_terminal();
        let workspace = &mut layout.workspaces[0];
        let first = workspace.tabs[0].id;
        let mut second = workspace.tabs[0].clone();
        second.id = Uuid::new_v4();
        second.label = "한글 review".into();
        let expected = second.id;
        workspace.tabs.push(second);
        let mut picker = TabPicker::new(workspace, first, |_| true).unwrap();
        picker.key("한글".as_bytes());
        assert!(matches!(picker.key(b"\r"), PickerAction::Select(id) if id == expected));
        picker.key(b"zzz");
        picker.key(b"\x1b[B");
        assert!(matches!(picker.key(b"\r"), PickerAction::Keep));
        assert!(
            picker
                .paint(Viewport::new(20, 3).unwrap())
                .unwrap()
                .contains("Tabs 0/0")
        );
        assert!(matches!(picker.key(b"\x1b"), PickerAction::Cancel));
    }

    #[test]
    fn search_does_not_open_a_closed_tab() {
        let layout = Layout::empty_terminal();
        let workspace = &layout.workspaces[0];
        let mut picker = TabPicker::new(workspace, workspace.tabs[0].id, |_| false).unwrap();
        picker.key(b"Shell");
        assert!(matches!(picker.key(b"\r"), PickerAction::Keep));
    }
}
