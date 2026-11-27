use std::path::Path;

use anyhow::{Result, ensure};
use gpui::{Global, Pixels, SharedString, TextSystem, font, px};

use crate::preferences::PreferencesStore;

pub const DEFAULT_FONT_SIZE: u16 = 15;
pub const FONT_SIZE: Pixels = px(DEFAULT_FONT_SIZE as f32);
pub const DEFAULT_FONT: &str = if cfg!(target_os = "macos") {
    "Menlo"
} else if cfg!(windows) {
    "Consolas"
} else {
    "DejaVu Sans Mono"
};

pub struct TerminalFonts {
    store: PreferencesStore,
    available: Vec<SharedString>,
    default: SharedString,
    selected: Option<SharedString>,
}

impl Global for TerminalFonts {}

impl TerminalFonts {
    pub fn load(state: &Path, text: &TextSystem) -> Self {
        let store = PreferencesStore::new(state);
        let available: Vec<SharedString> = text
            .all_font_names()
            .into_iter()
            .filter(|family| !family.starts_with('.') && is_monospaced(text, family))
            .map(SharedString::from)
            .collect();
        let default = available
            .iter()
            .find(|family| family.as_str() == DEFAULT_FONT)
            .or_else(|| available.first())
            .cloned()
            .unwrap_or_else(|| DEFAULT_FONT.into());
        let preference = store.load().unwrap_or_else(|error| {
            eprintln!("Could not read the Desktop terminal font preference: {error:#}");
            Default::default()
        });
        let selected = available
            .iter()
            .find(|family| Some(family.as_str()) == preference.font_family.as_deref())
            .cloned();

        Self {
            store,
            available,
            default,
            selected,
        }
    }

    pub fn current(&self) -> &SharedString {
        self.selected.as_ref().unwrap_or(&self.default)
    }

    pub fn default_family(&self) -> &SharedString {
        &self.default
    }

    pub fn selected(&self) -> Option<&SharedString> {
        self.selected.as_ref()
    }

    pub fn available(&self) -> &[SharedString] {
        &self.available
    }

    pub fn select(&mut self, family: Option<SharedString>) -> Result<()> {
        if self.selected == family {
            return Ok(());
        }
        ensure!(
            family
                .as_ref()
                .is_none_or(|name| self.available.contains(name)),
            "The selected terminal font is unavailable."
        );
        let font_family = family.as_ref().map(ToString::to_string);
        self.store.update(|preferences| {
            preferences.font_family = font_family;
        })?;
        self.selected = family;
        Ok(())
    }

    pub fn allows_clipboard_write(&self) -> bool {
        self.store.allows_clipboard_write()
    }

    pub fn set_clipboard_write(&mut self, allowed: bool) -> Result<()> {
        self.store.update(|preferences| {
            preferences.allow_clipboard_write = allowed;
        })?;
        Ok(())
    }
}

fn is_monospaced(text: &TextSystem, family: &str) -> bool {
    let id = text.resolve_font(&font(family.to_owned()));
    let Ok(reference) = text.advance(id, FONT_SIZE, 'M') else {
        return false;
    };
    reference.width > px(0.)
        && (' '..='~').all(|ch| {
            text.advance(id, FONT_SIZE, ch)
                .is_ok_and(|advance| (advance.width - reference.width).abs() < px(0.01))
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn saves_font_choice_and_restores_default() -> anyhow::Result<()> {
        use super::*;

        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("desktop-preferences");
        let store = PreferencesStore::from_dir(path);
        let mut fonts = TerminalFonts {
            store: store.clone(),
            available: vec!["Other Mono".into()],
            default: DEFAULT_FONT.into(),
            selected: None,
        };
        assert!(fonts.select(Some("Unavailable".into())).is_err());
        assert_eq!(fonts.current().as_str(), DEFAULT_FONT);

        assert!(!store.load()?.allow_clipboard_write);
        fonts.set_clipboard_write(true)?;

        fonts.select(Some("Other Mono".into()))?;
        assert_eq!(fonts.current().as_str(), "Other Mono");
        assert_eq!(store.load()?.font_family.as_deref(), Some("Other Mono"));

        fonts.select(None)?;
        assert_eq!(fonts.current().as_str(), DEFAULT_FONT);
        let saved = store.load()?;
        assert!(saved.font_family.is_none());
        assert!(saved.allow_clipboard_write);
        fonts.set_clipboard_write(false)?;
        assert!(!store.load()?.allow_clipboard_write);
        Ok(())
    }

    #[test]
    fn separate_instances_merge_fields_and_observe_revoked_clipboard_permission()
    -> anyhow::Result<()> {
        use super::*;

        let temporary = tempfile::tempdir()?;
        let store = PreferencesStore::new(temporary.path());
        let mut process_a = TerminalFonts {
            store: store.clone(),
            available: vec!["Other Mono".into()],
            default: DEFAULT_FONT.into(),
            selected: None,
        };
        let mut process_b = TerminalFonts {
            store: store.clone(),
            available: vec!["Other Mono".into()],
            default: DEFAULT_FONT.into(),
            selected: None,
        };

        process_a.set_clipboard_write(true)?;
        assert!(process_b.allows_clipboard_write());

        process_b.select(Some("Other Mono".into()))?;
        let saved = store.load()?;
        assert_eq!(saved.font_family.as_deref(), Some("Other Mono"));
        assert!(saved.allow_clipboard_write);

        process_a.set_clipboard_write(false)?;
        assert!(!process_b.allows_clipboard_write());
        Ok(())
    }
}
