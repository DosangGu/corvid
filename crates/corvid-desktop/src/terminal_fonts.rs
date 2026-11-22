use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
#[cfg(unix)]
use corvid_protocol::unix::RuntimeDir as Directory;
#[cfg(windows)]
use corvid_protocol::windows::StateDirectory as Directory;
use gpui::{Global, Pixels, SharedString, TextSystem, font, px};
use serde::{Deserialize, Serialize};

pub const DEFAULT_FONT_SIZE: u16 = 15;
pub const FONT_SIZE: Pixels = px(DEFAULT_FONT_SIZE as f32);
pub const DEFAULT_FONT: &str = if cfg!(target_os = "macos") {
    "Menlo"
} else if cfg!(windows) {
    "Consolas"
} else {
    "DejaVu Sans Mono"
};

const FILE: &str = "terminal.json";
const MAX_BYTES: u64 = 4096;

#[derive(Default, Serialize, Deserialize)]
struct Preferences {
    font_family: Option<String>,
    #[serde(default)]
    allow_clipboard_write: bool,
}

pub struct TerminalFonts {
    path: PathBuf,
    available: Vec<SharedString>,
    default: SharedString,
    selected: Option<SharedString>,
    allow_clipboard_write: bool,
}

impl Global for TerminalFonts {}

impl TerminalFonts {
    pub fn load(state: &Path, text: &TextSystem) -> Self {
        let path = state.join("desktop-preferences");
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
        let preference = read_preferences(&path).unwrap_or_else(|error| {
            eprintln!("Could not read the Desktop terminal font preference: {error:#}");
            Preferences::default()
        });
        let selected = available
            .iter()
            .find(|family| Some(family.as_str()) == preference.font_family.as_deref())
            .cloned();

        Self {
            path,
            available,
            default,
            selected,
            allow_clipboard_write: preference.allow_clipboard_write,
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
        self.save(family, self.allow_clipboard_write)
    }

    pub fn allows_clipboard_write(&self) -> bool {
        self.allow_clipboard_write
    }

    pub fn set_clipboard_write(&mut self, allowed: bool) -> Result<()> {
        self.save(self.selected.clone(), allowed)
    }

    fn save(&mut self, family: Option<SharedString>, allow_clipboard_write: bool) -> Result<()> {
        let preferences = Preferences {
            font_family: family.as_ref().map(ToString::to_string),
            allow_clipboard_write,
        };
        let directory = Directory::create_all(&self.path)?;
        directory.replace_private_file(FILE, &serde_json::to_vec(&preferences)?)?;
        self.selected = family;
        self.allow_clipboard_write = allow_clipboard_write;
        Ok(())
    }
}

fn read_preferences(path: &Path) -> Result<Preferences> {
    if path.try_exists()?
        && let Some(bytes) = Directory::open(path)?.read_private_file(FILE, MAX_BYTES)?
    {
        return Ok(serde_json::from_slice(&bytes)?);
    }
    Ok(Preferences::default())
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
        let mut fonts = TerminalFonts {
            path: path.clone(),
            available: vec!["Other Mono".into()],
            default: DEFAULT_FONT.into(),
            selected: None,
            allow_clipboard_write: false,
        };
        assert!(fonts.select(Some("Unavailable".into())).is_err());
        assert_eq!(fonts.current().as_str(), DEFAULT_FONT);

        assert!(!read_preferences(&path)?.allow_clipboard_write);
        fonts.set_clipboard_write(true)?;

        fonts.select(Some("Other Mono".into()))?;
        assert_eq!(fonts.current().as_str(), "Other Mono");
        assert_eq!(
            read_preferences(&path)?.font_family.as_deref(),
            Some("Other Mono")
        );

        fonts.select(None)?;
        assert_eq!(fonts.current().as_str(), DEFAULT_FONT);
        let saved = read_preferences(&path)?;
        assert!(saved.font_family.is_none());
        assert!(saved.allow_clipboard_write);
        fonts.set_clipboard_write(false)?;
        assert!(!read_preferences(&path)?.allow_clipboard_write);
        Ok(())
    }
}
