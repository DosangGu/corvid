use std::path::{Path, PathBuf};

use anyhow::Result;
#[cfg(unix)]
use corvid_protocol::unix::RuntimeDir as Directory;
#[cfg(windows)]
use corvid_protocol::windows::StateDirectory as Directory;
use gpui::{App, Global, KeyDownEvent, Pixels, PromptLevel, Rems, Window, px, rems};
use serde::{Deserialize, Serialize};

use crate::terminal_fonts::DEFAULT_FONT_SIZE;

const FILE: &str = "appearance.json";
const BASE_REM: f32 = 16.;
const APP_DEFAULT: u16 = 100;
pub const TERMINAL_SHORTCUTS: &str = if cfg!(target_os = "macos") {
    "Cmd++ / Cmd+− / Cmd+0"
} else {
    "Ctrl++ / Ctrl+− / Ctrl+0"
};

/// A layout length expressed in pixels at the default app scale.
pub fn ui_px(value: f32) -> Rems {
    rems(value / BASE_REM)
}

#[derive(Clone, Copy)]
pub enum Scope {
    App,
    Terminal,
}

impl Scope {
    fn limits(self) -> (u16, u16, u16, u16) {
        match self {
            Self::App => (80, 140, 10, APP_DEFAULT),
            Self::Terminal => (8, 32, 1, DEFAULT_FONT_SIZE),
        }
    }
}

#[derive(Clone, Copy)]
pub enum Change {
    Decrease,
    Reset,
    Increase,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
struct Sizes {
    app_percent: u16,
    terminal_px: u16,
}

impl Default for Sizes {
    fn default() -> Self {
        Self {
            app_percent: APP_DEFAULT,
            terminal_px: DEFAULT_FONT_SIZE,
        }
    }
}

impl Sizes {
    fn get_mut(&mut self, scope: Scope) -> &mut u16 {
        match scope {
            Scope::App => &mut self.app_percent,
            Scope::Terminal => &mut self.terminal_px,
        }
    }
}

pub struct Appearance {
    path: PathBuf,
    sizes: Sizes,
}

impl Global for Appearance {}

impl Appearance {
    pub fn load(state: &Path) -> Self {
        let path = state.join("desktop-preferences");
        let mut sizes = Self::read(&path).unwrap_or_else(|error| {
            eprintln!("Could not read Desktop appearance preferences: {error:#}");
            Sizes::default()
        });
        for scope in [Scope::App, Scope::Terminal] {
            let (min, max, _, _) = scope.limits();
            let value = sizes.get_mut(scope);
            *value = (*value).clamp(min, max);
        }
        Self { path, sizes }
    }

    fn read(path: &Path) -> Result<Sizes> {
        if path.try_exists()?
            && let Some(bytes) = Directory::open(path)?.read_private_file(FILE, 4096)?
        {
            return Ok(serde_json::from_slice(&bytes)?);
        }
        Ok(Sizes::default())
    }

    pub fn value(&self, scope: Scope) -> u16 {
        match scope {
            Scope::App => self.sizes.app_percent,
            Scope::Terminal => self.sizes.terminal_px,
        }
    }

    pub fn target(&self, scope: Scope, change: Change) -> u16 {
        let value = self.value(scope);
        let (min, max, step, default) = scope.limits();
        match change {
            Change::Decrease => value.saturating_sub(step).max(min),
            Change::Increase => (value + step).min(max),
            Change::Reset => default,
        }
    }

    pub fn rem_size(&self) -> Pixels {
        px(BASE_REM * self.scale())
    }

    pub fn terminal_size(&self) -> Pixels {
        px(f32::from(self.sizes.terminal_px) * self.scale())
    }

    fn scale(&self) -> f32 {
        f32::from(self.sizes.app_percent) / f32::from(APP_DEFAULT)
    }

    fn save(&mut self, scope: Scope, value: u16) -> Result<()> {
        let mut sizes = self.sizes;
        *sizes.get_mut(scope) = value;
        let directory = Directory::create_all(&self.path)?;
        directory.replace_private_file(FILE, &serde_json::to_vec(&sizes)?)?;
        self.sizes = sizes;
        Ok(())
    }
}

pub fn adjust(scope: Scope, change: Change, window: &mut Window, cx: &mut App) {
    let appearance = cx.global::<Appearance>();
    let target = appearance.target(scope, change);
    if target == appearance.value(scope) {
        return;
    }
    match cx.global_mut::<Appearance>().save(scope, target) {
        Ok(()) => cx.refresh_windows(),
        Err(error) => {
            drop(window.prompt(
                PromptLevel::Warning,
                "Could not save the appearance settings",
                Some(&format!("{error:#}")),
                &["OK"],
                cx,
            ));
        }
    }
}

pub fn terminal_shortcut(event: &KeyDownEvent, window: &mut Window, cx: &mut App) -> bool {
    let Some(change) = terminal_change(&event.keystroke) else {
        return false;
    };
    if !event.is_held {
        adjust(Scope::Terminal, change, window, cx);
    }
    cx.stop_propagation();
    true
}

fn terminal_change(key: &gpui::Keystroke) -> Option<Change> {
    let modifiers = key.modifiers;
    let primary = if cfg!(target_os = "macos") {
        modifiers.platform && !modifiers.control
    } else {
        modifiers.control && !modifiers.platform
    };
    if !primary || modifiers.alt {
        return None;
    }
    match (key.key.as_str(), modifiers.shift) {
        ("+" | "=" | "add", _) => Some(Change::Increase),
        ("-" | "subtract", false) => Some(Change::Decrease),
        ("0", false) => Some(Change::Reset),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Keystroke, Modifiers};

    #[test]
    fn zoom_leaves_shifted_minus_and_other_terminal_input_unclaimed() {
        let primary = Modifiers {
            control: !cfg!(target_os = "macos"),
            platform: cfg!(target_os = "macos"),
            ..Modifiers::default()
        };
        let key = |name: &str, modifiers| Keystroke {
            key: name.into(),
            modifiers,
            key_char: None,
        };
        assert!(terminal_change(&key("_", primary)).is_none());
        let shifted = Modifiers {
            shift: true,
            ..primary
        };
        assert!(terminal_change(&key("-", shifted)).is_none());
        assert!(matches!(
            terminal_change(&key("+", shifted)),
            Some(Change::Increase)
        ));
        let alt = Modifiers {
            alt: true,
            ..primary
        };
        assert!(terminal_change(&key("-", alt)).is_none());
    }

    #[test]
    fn restores_app_and_terminal_sizes_and_resets_them_independently() -> Result<()> {
        let state = tempfile::tempdir()?;
        let mut appearance = Appearance::load(state.path());
        appearance.save(Scope::App, 120)?;
        appearance.save(Scope::Terminal, 20)?;

        let mut reopened = Appearance::load(state.path());
        assert_eq!(reopened.value(Scope::App), 120);
        assert_eq!(reopened.value(Scope::Terminal), 20);
        assert_eq!(reopened.terminal_size(), px(24.));

        let default = reopened.target(Scope::Terminal, Change::Reset);
        reopened.save(Scope::Terminal, default)?;
        assert_eq!(reopened.value(Scope::App), 120);
        assert_eq!(reopened.terminal_size(), px(18.));
        Ok(())
    }
}
