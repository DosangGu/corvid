use std::path::Path;

use anyhow::Result;
use eframe::egui::{self, ViewportCommand, WindowLevel};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
use corvid_protocol::unix::RuntimeDir as StateDirectory;
#[cfg(windows)]
use corvid_protocol::windows::StateDirectory;

mod layout;
#[cfg(windows)]
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

pub(crate) struct Window {
    storage: StateDirectory,
    preferences: Preferences,
    pub expanded: bool,
    pub error: Option<String>,
    background_start: bool,
    applied: Option<Applied>,
    was_focused: bool,
    anchor_frames: u8,
    anchor_at_cursor: bool,
    #[cfg(windows)]
    native: Option<native::NativeWindow>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Applied {
    expanded: bool,
    topmost: bool,
    layout: GridLayout,
    fixed_position: bool,
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
            anchor_frames: 0,
            anchor_at_cursor: false,
            #[cfg(windows)]
            native: None,
        })
    }

    pub fn always_on_top(&self) -> bool {
        self.preferences.always_on_top
    }

    #[cfg(windows)]
    pub fn bind_native(&mut self, creation: &eframe::CreationContext<'_>) {
        self.native = native::NativeWindow::bind(creation);
    }

    pub fn layout(&self) -> GridLayout {
        self.preferences.layout
    }

    pub fn fixed_position(&self) -> bool {
        self.preferences.fixed_position
    }

    pub fn set_fixed_position(&mut self, fixed_position: bool) {
        let next = Preferences {
            fixed_position,
            ..self.preferences.clone()
        };
        if self.save(next) {
            self.anchor_frames = if fixed_position { 2 } else { 0 };
            self.anchor_at_cursor = false;
        }
    }

    pub fn start_drag(&mut self, ctx: &egui::Context) {
        if self.fixed_position() {
            return;
        }
        self.anchor_frames = 0;
        self.anchor_at_cursor = false;
        #[cfg(windows)]
        if self.native.is_some() {
            return;
        }
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }

    pub fn drag(&self, ctx: &egui::Context) {
        #[cfg(windows)]
        if !self.fixed_position()
            && let Some(native) = &self.native
        {
            native.drag(ctx);
        }
        #[cfg(not(windows))]
        let _ = ctx;
    }

    pub fn set_layout(&mut self, layout: GridLayout) {
        let next = Preferences {
            layout,
            ..self.preferences.clone()
        };
        self.save(next);
    }

    pub fn toggle_topmost(&mut self, ctx: &egui::Context, tray: bool) {
        let next = Preferences {
            always_on_top: !self.always_on_top(),
            ..self.preferences.clone()
        };
        if self.save(next) && tray && !self.always_on_top() {
            self.hide(ctx);
        }
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

    pub fn hide(&mut self, ctx: &egui::Context) {
        self.was_focused = false;
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
    }

    pub fn update(&mut self, ctx: &egui::Context, frame: &eframe::Frame, tray: bool, opened: bool) {
        let first = self.applied.is_none();
        let state = Applied {
            expanded: self.expanded,
            topmost: self.always_on_top(),
            layout: self.layout(),
            fixed_position: self.fixed_position(),
        };
        let mode_changed = self
            .applied
            .is_none_or(|previous| previous.expanded != self.expanded);
        let level_changed = self
            .applied
            .is_none_or(|previous| previous.topmost != state.topmost);
        let size_changed = mode_changed
            || (!self.expanded
                && self
                    .applied
                    .is_none_or(|previous| previous.layout != state.layout));
        if level_changed {
            ctx.send_viewport_cmd(ViewportCommand::WindowLevel(if self.always_on_top() {
                WindowLevel::AlwaysOnTop
            } else {
                WindowLevel::Normal
            }));
        }
        if self.applied != Some(state) {
            if mode_changed {
                ctx.send_viewport_cmd(ViewportCommand::Decorations(self.expanded));
                ctx.send_viewport_cmd(ViewportCommand::Resizable(self.expanded));
                ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(if self.expanded {
                    egui::vec2(440.0, 360.0)
                } else {
                    egui::vec2(204.0, 140.0)
                }));
            }
            if size_changed {
                ctx.send_viewport_cmd(ViewportCommand::InnerSize(if self.expanded {
                    egui::vec2(800.0, 560.0)
                } else {
                    self.layout().size()
                }));
                self.anchor_frames = 2;
            }
            self.applied = Some(state);
        } else if self.anchor_frames > 0 {
            place(ctx, frame, self.fixed_position(), self.anchor_at_cursor);
            self.anchor_at_cursor = false;
            self.anchor_frames -= 1;
        }
        if first || opened {
            self.was_focused = false;
            if first || (self.fixed_position() && !self.always_on_top()) {
                self.anchor_frames = 2;
                self.anchor_at_cursor = true;
            }
        }
        if first {
            place(ctx, frame, true, true);
        }
        if first && (!self.background_start || self.always_on_top() || !tray) {
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            if !self.background_start {
                ctx.send_viewport_cmd(ViewportCommand::Focus);
            }
        }
        let focused = ctx.input(|input| input.focused);
        if dismiss_on_blur(tray, self.always_on_top(), self.was_focused, focused) && !opened {
            self.hide(ctx);
        }
        self.was_focused = focused;
        if self.anchor_frames > 0 {
            ctx.request_repaint();
        }
    }
}

pub(crate) fn request_quit(ctx: &egui::Context) {
    let first = ctx.data_mut(|data| {
        let id = egui::Id::new("quit-agent-deck");
        let first = !data.get_temp::<bool>(id).unwrap_or(false);
        data.insert_temp(id, true);
        first
    });
    ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, ViewportCommand::Close);
    ctx.request_repaint_of(egui::ViewportId::ROOT);
    #[cfg(windows)]
    if first {
        native::wake_for_quit(ctx);
    }
    #[cfg(not(windows))]
    let _ = first;
}

pub(crate) fn quit_requested(ctx: &egui::Context) -> bool {
    ctx.data(|data| data.get_temp::<bool>(egui::Id::new("quit-agent-deck")))
        .unwrap_or(false)
}

fn dismiss_on_blur(tray: bool, topmost: bool, was_focused: bool, focused: bool) -> bool {
    tray && !topmost && was_focused && !focused
}

pub(crate) fn request_open(ctx: &egui::Context) {
    ctx.data_mut(|data| data.insert_temp(egui::Id::new("open-agent-deck"), true));
}

pub(crate) fn take_open_request(ctx: &egui::Context) -> bool {
    ctx.data_mut(|data| data.remove_temp::<bool>(egui::Id::new("open-agent-deck")))
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn place(ctx: &egui::Context, _: &eframe::Frame, fixed: bool, _: bool) {
    // Wayland controls placement itself. X11 can honor the current screen size.
    if fixed && let Some(command) = egui::ViewportCommand::center_on_screen(ctx) {
        ctx.send_viewport_cmd(command);
    }
}

#[cfg(windows)]
fn place(_: &egui::Context, frame: &eframe::Frame, fixed: bool, at_cursor: bool) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, MonitorFromWindow,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetWindowRect, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
    };

    let Ok(handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let hwnd = handle.hwnd.get() as *mut std::ffi::c_void;
    // The frame owns this HWND for the duration of the call. Work-area coordinates
    // and window bounds use native pixels, including negative monitor origins.
    unsafe {
        let mut cursor = POINT::default();
        let monitor = if at_cursor && GetCursorPos(&mut cursor) != 0 {
            MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST)
        } else {
            MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)
        };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let mut rect = RECT::default();
        if GetMonitorInfoW(monitor, &mut info) == 0 || GetWindowRect(hwnd, &mut rect) == 0 {
            return;
        }
        let (x, y) = window_position(
            [
                info.rcWork.left,
                info.rcWork.top,
                info.rcWork.right,
                info.rcWork.bottom,
            ],
            [rect.right - rect.left, rect.bottom - rect.top],
            fixed,
            [rect.left, rect.top],
        );
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
        );
    }
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
    fn dragging_requires_movable_mode_and_cancels_pending_placement() -> Result<()> {
        #[cfg(target_os = "linux")]
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
        #[cfg(windows)]
        let root = tempfile::tempdir()?;
        let mut window = Window::load(root.path(), false)?;
        let ctx = egui::Context::default();
        window.set_fixed_position(true);
        let output = ctx.run(egui::RawInput::default(), |ctx| window.start_drag(ctx));
        assert!(
            !output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&ViewportCommand::StartDrag)
        );
        window.set_fixed_position(false);
        window.anchor_frames = 2;
        let output = ctx.run(egui::RawInput::default(), |ctx| window.start_drag(ctx));
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&ViewportCommand::StartDrag)
        );
        assert_eq!(window.anchor_frames, 0);
        Ok(())
    }

    #[test]
    fn quit_remains_requested_across_frames_and_reopen_requests() {
        let ctx = egui::Context::default();
        request_quit(&ctx);
        request_open(&ctx);
        assert!(take_open_request(&ctx));
        for _ in 0..2 {
            let output = ctx.run(egui::RawInput::default(), |ctx| {
                assert!(quit_requested(ctx));
                request_quit(ctx);
            });
            let commands = &output.viewport_output[&egui::ViewportId::ROOT].commands;
            assert!(commands.contains(&ViewportCommand::Close));
            assert!(!commands.contains(&ViewportCommand::CancelClose));
            assert!(!commands.contains(&ViewportCommand::Focus));
        }
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
        window.toggle_topmost(&egui::Context::default(), true);
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
