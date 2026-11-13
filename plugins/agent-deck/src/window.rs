use std::path::Path;

use crate::ui_signal::UiSignal;
use anyhow::Result;
use gpui::{px, size};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
use corvid_protocol::unix::RuntimeDir as StateDirectory;
#[cfg(windows)]
use corvid_protocol::windows::StateDirectory;

mod layout;
mod native;
pub(crate) use layout::{BAR_HEIGHT, GAP, GridLayout, KEY_HEIGHT, KEY_WIDTH, Orientation, Preset};

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
struct Preferences {
    always_on_top: bool,
    fixed_position: bool,
    layout: GridLayout,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            always_on_top: true,
            fixed_position: false,
            layout: GridLayout::default(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Applied {
    expanded: bool,
    always_on_top: bool,
    layout: GridLayout,
    fixed_position: bool,
}

pub(crate) struct Window {
    storage: StateDirectory,
    preferences: Preferences,
    pub expanded: bool,
    pub error: Option<String>,
    background_start: bool,
    applied: Option<Applied>,
    was_focused: bool,
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
            was_focused: false,
        })
    }

    pub fn always_on_top(&self) -> bool {
        self.preferences.always_on_top
    }
    pub fn fixed_position(&self) -> bool {
        self.preferences.fixed_position
    }
    pub fn layout(&self) -> GridLayout {
        self.preferences.layout
    }

    pub fn set_fixed_position(&mut self, fixed_position: bool) {
        self.save(Preferences {
            fixed_position,
            ..self.preferences.clone()
        });
    }

    pub fn set_layout(&mut self, layout: GridLayout) {
        self.save(Preferences {
            layout,
            ..self.preferences.clone()
        });
    }

    pub fn toggle_topmost(&mut self) {
        self.save(Preferences {
            always_on_top: !self.always_on_top(),
            ..self.preferences.clone()
        });
    }

    fn save(&mut self, next: Preferences) -> bool {
        let result = serde_json::to_vec(&next)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| self.storage.replace_private_file("window.json", &bytes));
        if let Err(error) = result {
            self.error = Some(format!("Could not save window preference: {error}"));
            return false;
        }
        self.preferences = next;
        self.error = None;
        true
    }

    pub fn hide(&mut self, window: &gpui::Window) {
        self.was_focused = false;
        if let Err(error) = native::visible(window, false) {
            self.error = Some(error.to_string());
        }
    }

    pub fn update(&mut self, window: &mut gpui::Window, tray: bool, opened: bool) {
        let first = self.applied.is_none();
        let state = Applied {
            expanded: self.expanded,
            always_on_top: self.always_on_top(),
            layout: self.layout(),
            fixed_position: self.fixed_position(),
        };
        if self.applied != Some(state) {
            let bounds_changed = self
                .applied
                .is_none_or(|old| old.expanded != state.expanded || old.layout != state.layout);
            let result =
                native::configure(window, self.always_on_top(), self.expanded).and_then(|_| {
                    if bounds_changed {
                        let dimensions = if self.expanded {
                            size(px(800.), px(560.))
                        } else {
                            self.layout().size()
                        };
                        if window.viewport_size() != dimensions {
                            window.resize(dimensions);
                        }
                    }
                    native::place(window, first || self.fixed_position(), first)
                });
            match result {
                Ok(()) => {
                    self.applied = Some(state);
                    self.error = None;
                }
                Err(error) => self.error = Some(error.to_string()),
            }
        }
        if opened
            && self.fixed_position()
            && let Err(error) = native::place(window, true, true)
        {
            self.error = Some(error.to_string());
        }
        if opened || (first && (!self.background_start || self.always_on_top() || !tray)) {
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
        let focused = window.is_window_active();
        if dismiss_on_blur(tray, self.always_on_top(), self.was_focused, focused) && !opened {
            self.hide(window);
        }
        self.was_focused = focused;
    }
}

pub(crate) fn request_quit(signal: &UiSignal) {
    signal.quit();
}
pub(crate) fn request_open(signal: &UiSignal) {
    signal.open();
}

fn dismiss_on_blur(tray: bool, topmost: bool, was_focused: bool, focused: bool) -> bool {
    tray && !topmost && was_focused && !focused
}

#[cfg(any(windows, test))]
fn bottom_right(work: [i32; 4], size: [i32; 2], gap: i32) -> (i32, i32) {
    (
        (work[2] - size[0] - gap).max(work[0]),
        (work[3] - size[1] - gap).max(work[1]),
    )
}

#[cfg(any(windows, test))]
fn window_position(work: [i32; 4], size: [i32; 2], fixed: bool, current: [i32; 2]) -> (i32, i32) {
    if fixed {
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
    fn popup_hides_only_after_focus_leaves_with_a_working_tray() {
        assert!(dismiss_on_blur(true, false, true, false));
        assert!(!dismiss_on_blur(true, true, true, false));
        assert!(!dismiss_on_blur(false, false, true, false));
        assert!(!dismiss_on_blur(true, false, false, false));
        assert!(!dismiss_on_blur(true, false, true, true));
    }

    #[test]
    fn anchors_inside_work_area_with_negative_monitor_coordinates() {
        assert_eq!(
            bottom_right([-1920, 0, 0, 1040], [392, 244], 12),
            (-404, 784)
        );
        assert_eq!(bottom_right([0, 0, 320, 200], [392, 244], 12), (0, 0));
    }

    #[test]
    fn movable_window_keeps_its_location_and_only_shifts_to_fit() {
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
    fn layout_and_topmost_choices_survive_restart() -> Result<()> {
        #[cfg(target_os = "linux")]
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
        #[cfg(windows)]
        let root = tempfile::tempdir()?;
        let mut window = Window::load(root.path(), false)?;
        assert!(window.always_on_top());
        let layout = GridLayout {
            preset: Preset::Fifteen,
            orientation: Orientation::Portrait,
        };
        window.set_layout(layout);
        window.set_fixed_position(true);
        window.toggle_topmost();
        assert!(window.error.is_none());
        let saved = Window::load(root.path(), true)?;
        assert!(!saved.always_on_top());
        assert!(saved.fixed_position());
        assert!(saved.layout() == layout);
        Ok(())
    }

    #[test]
    fn existing_preferences_keep_visibility_and_default_to_eight_landscape_keys() -> Result<()> {
        let preferences: Preferences = serde_json::from_str(r#"{"always_on_top":false}"#)?;
        assert!(!preferences.always_on_top);
        assert!(!preferences.fixed_position);
        assert_eq!(preferences.layout.dimensions(), (4, 2));
        Ok(())
    }
}
