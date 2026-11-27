use crate::layout::clean_title;

/// Outcome of submitting a tab rename editor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameOutcome {
    /// Keep the existing title and naming mode unchanged.
    Unchanged,
    /// Persist a new manual tab name.
    SetManual(String),
    /// Clear the manual override and return to automatic titles.
    ResetAutomatic,
}

/// Shared tab rename editor state across Desktop and TUI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameState {
    initial_manual: Option<String>,
    text: String,
    edited: bool,
}

impl RenameState {
    pub fn new(display_label: &str, manual_name: Option<&str>) -> Self {
        let initial_manual = manual_name.map(clean_title).filter(|name| !name.is_empty());
        Self {
            initial_manual,
            text: clean_title(display_label),
            edited: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn append(&mut self, text: &str) {
        self.edited = true;
        self.text = clean_title(&(self.text.clone() + text));
    }

    pub fn replace(&mut self, text: &str) {
        self.edited = true;
        self.text = clean_title(text);
    }

    pub fn pop(&mut self) {
        self.edited = true;
        self.text.pop();
    }

    pub fn clear(&mut self) {
        self.edited = true;
        self.text.clear();
    }

    pub fn submit(&self) -> RenameOutcome {
        if !self.edited {
            return RenameOutcome::Unchanged;
        }
        if self.text.is_empty() {
            return if self.initial_manual.is_some() {
                RenameOutcome::ResetAutomatic
            } else {
                RenameOutcome::Unchanged
            };
        }
        if self.initial_manual.as_deref() == Some(self.text.as_str()) {
            return RenameOutcome::Unchanged;
        }
        RenameOutcome::SetManual(self.text.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_submit_preserves_manual_and_automatic_modes() {
        let manual = RenameState::new("Release notes", Some("Release notes"));
        assert_eq!(manual.text(), "Release notes");
        assert_eq!(manual.submit(), RenameOutcome::Unchanged);

        let automatic = RenameState::new("Pane 1", None);
        assert_eq!(automatic.text(), "Pane 1");
        assert_eq!(automatic.submit(), RenameOutcome::Unchanged);
    }

    #[test]
    fn editing_to_same_manual_name_is_unchanged_and_clearing_resets_manual_only() {
        let mut manual = RenameState::new("Release notes", Some("Release notes"));
        manual.pop();
        manual.append("s");
        assert_eq!(manual.submit(), RenameOutcome::Unchanged);

        manual.clear();
        assert_eq!(manual.submit(), RenameOutcome::ResetAutomatic);

        let mut automatic = RenameState::new("Pane 1", None);
        automatic.clear();
        assert_eq!(automatic.submit(), RenameOutcome::Unchanged);
        automatic.append("Build");
        assert_eq!(automatic.submit(), RenameOutcome::SetManual("Build".into()));
    }
}
