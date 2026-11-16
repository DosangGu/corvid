#[cfg(target_os = "linux")]
#[path = "x11.rs"]
mod implementation;

#[cfg(windows)]
#[path = "windows.rs"]
mod implementation;

pub(super) use implementation::{configure, set_bounds};

#[cfg(windows)]
pub(super) use implementation::{hide, show, start_move};

#[cfg(not(windows))]
pub(super) fn hide(window: &gpui::Window) -> anyhow::Result<()> {
    implementation::visible(window, false)
}

#[cfg(not(windows))]
pub(super) fn show(window: &gpui::Window, activate: bool, _: &gpui::App) -> anyhow::Result<()> {
    implementation::visible(window, true)?;
    if activate {
        window.activate_window();
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod implementation;
