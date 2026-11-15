use anyhow::{Result, bail};
use gpui::{Pixels, Size, Window};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::ffi::c_void;

unsafe extern "C" {
    fn corvid_window_visible(view: *mut c_void, visible: bool);
    fn corvid_window_configure(view: *mut c_void, pinned: bool, expanded: bool);
    fn corvid_window_bounds(view: *mut c_void, width: f64, height: f64, initial: bool);
}

fn view(window: &Window) -> Result<*mut c_void> {
    let handle = HasWindowHandle::window_handle(window)
        .map_err(|error| anyhow::anyhow!("reading AppKit window handle: {error}"))?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        bail!("Agent Deck requires an AppKit window");
    };
    Ok(handle.ns_view.as_ptr())
}

pub fn visible(window: &Window, visible: bool) -> Result<()> {
    // GPUI owns the NSView and calls window operations on the AppKit thread.
    unsafe { corvid_window_visible(view(window)?, visible) };
    Ok(())
}

pub fn configure(window: &Window, pinned: bool, expanded: bool) -> Result<()> {
    unsafe { corvid_window_configure(view(window)?, pinned, expanded) };
    Ok(())
}

pub fn set_bounds(window: &mut Window, size: Size<Pixels>, initial: bool) -> Result<()> {
    unsafe {
        corvid_window_bounds(
            view(window)?,
            f32::from(size.width) as f64,
            f32::from(size.height) as f64,
            initial,
        );
    }
    Ok(())
}
