use corvid_core::Viewport;
use corvid_core::rename::{RenameOutcome, RenameState};

pub(super) struct NameEditor {
    state: RenameState,
    pub(super) dirty: bool,
}

pub(super) enum Edit {
    Keep,
    Cancel,
    Save(String),
}

impl NameEditor {
    pub(super) fn new(display_label: &str, manual_name: Option<&str>) -> Self {
        Self {
            state: RenameState::new(display_label, manual_name),
            dirty: true,
        }
    }

    pub(super) fn key(&mut self, bytes: &[u8]) -> Edit {
        match bytes {
            b"\x1b" | b"\x03" => return Edit::Cancel,
            b"\r" | b"\n" => {
                return match self.state.submit() {
                    RenameOutcome::Unchanged => Edit::Cancel,
                    RenameOutcome::SetManual(name) => Edit::Save(name),
                    RenameOutcome::ResetAutomatic => Edit::Save(String::new()),
                };
            }
            b"\x7f" | b"\x08" => {
                self.state.pop();
            }
            b"\x15" => self.state.clear(),
            _ => {
                if let Ok(text) = std::str::from_utf8(bytes)
                    && text.chars().all(|ch| !ch.is_control())
                {
                    self.append(text);
                }
            }
        }
        self.dirty = true;
        Edit::Keep
    }

    pub(super) fn append(&mut self, text: &str) {
        self.state.append(text);
        self.dirty = true;
    }

    pub(super) fn paint(&mut self, viewport: Viewport) -> String {
        let width = usize::from(viewport.cols.get().saturating_sub(1));
        let rows = viewport.rows.get();
        let row = rows.saturating_sub(2).max(1);
        self.dirty = false;
        let lines = [
            "Rename tab | Enter save | Esc cancel".to_owned(),
            format!("> {}", self.state.text()),
            "Empty name: automatic title | Ctrl-U: clear".to_owned(),
        ];
        let mut frame = String::from("\x1b[?25l\x1b[0m");
        for (offset, line) in lines.iter().take(usize::from(rows)).enumerate() {
            use std::fmt::Write;
            write!(
                frame,
                "\x1b[{};1H\x1b[2K{}",
                row + offset as u16,
                super::status::clip(line, width)
            )
            .expect("writing to a String");
        }
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_ignores_terminal_keys_and_saves_unicode_paste_as_text() {
        let mut editor = NameEditor::new("Pane 1", None);
        editor.key(b"\x1b[D");
        editor.key(b"\x15");
        editor.append("한글; build\r\n🐦");
        editor.key(b"\x7f");
        assert!(matches!(editor.key(b"\r"), Edit::Save(name) if name == "한글; build"));
        editor.key(b"\x15");
        assert!(matches!(editor.key(b"\r"), Edit::Cancel));
        assert!(matches!(editor.key(b"\x1b"), Edit::Cancel));
    }

    #[test]
    fn unchanged_enter_preserves_manual_and_automatic_tabs() {
        let mut manual = NameEditor::new("Release notes", Some("Release notes"));
        assert!(matches!(manual.key(b"\r"), Edit::Cancel));
        manual.key(b"\x15");
        assert!(matches!(manual.key(b"\r"), Edit::Save(name) if name.is_empty()));

        let mut automatic = NameEditor::new("Pane 1", None);
        assert!(matches!(automatic.key(b"\r"), Edit::Cancel));
    }
}
