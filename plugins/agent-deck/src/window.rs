use std::path::Path;

use crate::ui_signal::UiSignal;
use anyhow::Result;
use gpui::{px, size};
use serde::{Deserialize, Deserializer, Serialize};

#[cfg(target_os = "linux")]
use corvid_protocol::unix::RuntimeDir as StateDirectory;
#[cfg(windows)]
use corvid_protocol::windows::StateDirectory;

mod layout;
mod native;
pub(crate) use layout::{BAR_HEIGHT, GAP, GridLayout, KEY_HEIGHT, KEY_WIDTH, Orientation, Preset};

#[derive(Clone, Default, Serialize)]
struct Preferences {
    pinned: bool,
    layout: GridLayout,
}

impl<'de> Deserialize<'de> for Preferences {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Saved {
            pinned: Option<bool>,
            fixed_position: bool,
            layout: GridLayout,
        }

        let saved = Saved::deserialize(deserializer)?;
        Ok(Self {
            // Honor explicit position locks. The legacy default was movable and topmost.
            pinned: saved.pinned.unwrap_or(saved.fixed_position),
            layout: saved.layout,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Applied {
    expanded: bool,
    pinned: bool,
    layout: GridLayout,
}

pub(crate) struct Window {
    storage: StateDirectory,
    preferences: Preferences,
    pub expanded: bool,
    pub error: Option<String>,
    background_start: bool,
    applied: Option<Applied>,
}

impl Window {
    pub fn load(state: &Path, background_start: bool) -> Result<Self> {
        let storage = StateDirectory::create_all(&state.join("agent-deck"))?;
        let preferences = storage
            .read_private_file("window.json", 4096)?
            .map(|bytes| serde_json::from_slice(&bytes))
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            storage,
            preferences,
            expanded: false,
            error: None,
            background_start,
            applied: None,
        })
    }

    pub fn pinned(&self) -> bool {
        self.preferences.pinned
    }
    pub fn layout(&self) -> GridLayout {
        self.preferences.layout
    }

    pub fn set_layout(&mut self, layout: GridLayout) {
        self.save(Preferences {
            layout,
            ..self.preferences.clone()
        });
    }

    pub fn toggle_pin(&mut self) {
        self.save(Preferences {
            pinned: !self.pinned(),
            ..self.preferences.clone()
        });
    }

    fn save(&mut self, next: Preferences) {
        let result = serde_json::to_vec(&next)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| self.storage.replace_private_file("window.json", &bytes));
        if let Err(error) = result {
            self.error = Some(format!("Could not save window preference: {error}"));
            return;
        }
        self.preferences = next;
        self.error = None;
    }

    pub fn hide(&mut self, window: &gpui::Window) {
        if let Err(error) = native::visible(window, false) {
            self.error = Some(error.to_string());
        }
    }

    pub fn update(&mut self, window: &mut gpui::Window, tray: bool, opened: bool) {
        let first = self.applied.is_none();
        let state = Applied {
            expanded: self.expanded,
            pinned: self.pinned(),
            layout: self.layout(),
        };
        if self.applied != Some(state) {
            let result = self.apply(window, state);
            match result {
                Ok(()) => {
                    self.applied = Some(state);
                    self.error = None;
                }
                Err(error) => self.error = Some(error.to_string()),
            }
        }
        if opened || (first && (!self.background_start || self.pinned() || !tray)) {
            if let Err(error) = native::visible(window, true) {
                self.error = Some(error.to_string());
            }
            if opened || !self.background_start {
                window.activate_window();
            }
        }
        if first || opened {
            window.refresh();
        }
    }

    fn apply(&self, window: &mut gpui::Window, state: Applied) -> Result<()> {
        if self
            .applied
            .is_none_or(|old| old.pinned != state.pinned || old.expanded != state.expanded)
        {
            native::configure(window, state.pinned, state.expanded)?;
        }
        if self.applied.is_none_or(|old| {
            old.expanded != state.expanded || (!state.expanded && old.layout != state.layout)
        }) {
            let dimensions = if state.expanded {
                size(px(800.), px(560.))
            } else {
                state.layout.size()
            };
            native::set_bounds(window, dimensions, self.applied.is_none())?;
        }
        Ok(())
    }
}

pub(crate) fn request_quit(signal: &UiSignal) {
    signal.quit();
}
pub(crate) fn request_open(signal: &UiSignal) {
    signal.open();
}

fn bottom_right(work: [i32; 4], size: [i32; 2], gap: i32) -> (i32, i32) {
    (
        (work[2] - size[0] - gap).max(work[0]),
        (work[3] - size[1] - gap).max(work[1]),
    )
}

fn window_position(work: [i32; 4], size: [i32; 2], initial: bool, current: [i32; 2]) -> (i32, i32) {
    if initial {
        return bottom_right(work, size, 12);
    }
    (
        current[0].clamp(work[0], (work[2] - size[0]).max(work[0])),
        current[1].clamp(work[1], (work[3] - size[1]).max(work[1])),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_inside_work_area_with_negative_monitor_coordinates() {
        assert_eq!(
            bottom_right([-1920, 0, 0, 1040], [392, 244], 12),
            (-404, 784)
        );
        assert_eq!(bottom_right([0, 0, 320, 200], [392, 244], 12), (0, 0));
    }

    #[test]
    fn resizing_keeps_the_origin_and_only_shifts_to_fit() {
        let work = [-1920, 0, 0, 1040];
        assert_eq!(
            window_position(work, [392, 244], false, [-1500, 200]),
            (-1500, 200)
        );
        assert_eq!(
            window_position(work, [800, 560], false, [-404, 784]),
            (-800, 480)
        );
        assert_eq!(
            window_position(work, [392, 244], true, [-1500, 200]),
            (-404, 784)
        );
        assert_eq!(
            window_position([0, 0, 320, 200], [392, 244], false, [50, 50]),
            (0, 0)
        );
    }

    #[test]
    fn layout_and_pin_choices_survive_restart() -> Result<()> {
        #[cfg(target_os = "linux")]
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
        #[cfg(windows)]
        let root = tempfile::tempdir()?;
        let mut window = Window::load(root.path(), false)?;
        assert!(!window.pinned());
        let layout = GridLayout {
            preset: Preset::Fifteen,
            orientation: Orientation::Portrait,
        };
        window.set_layout(layout);
        window.toggle_pin();
        assert!(window.error.is_none());
        let saved = Window::load(root.path(), true)?;
        assert!(saved.pinned());
        assert!(saved.layout() == layout);
        Ok(())
    }

    #[test]
    fn legacy_position_lock_is_migrated_without_locking_movable_windows() -> Result<()> {
        for (json, pinned) in [
            (r#"{"always_on_top":true}"#, false),
            (r#"{"always_on_top":false,"fixed_position":true}"#, true),
            (r#"{"pinned":false,"fixed_position":true}"#, false),
        ] {
            let preferences: Preferences = serde_json::from_str(json)?;
            assert_eq!(preferences.pinned, pinned);
            assert_eq!(preferences.layout.dimensions(), (4, 2));
        }
        Ok(())
    }
}
