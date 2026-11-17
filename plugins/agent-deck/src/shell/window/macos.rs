use std::ffi::c_void;

use anyhow::{Result, bail};
use gpui::{Pixels, Size, Window};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::ui_signal::Presentation;

unsafe extern "C" {
    fn corvid_panel_prepare();
    fn corvid_panel_hide(view: *mut c_void);
    fn corvid_panel_update(view: *mut c_void, width: f64, height: f64, presentation: u8);
}

pub(in crate::shell) fn prepare() {
    // GPUI runs the launch callback on the AppKit thread after setting its
    // normal activation policy. The Deck is an accessory app.
    unsafe { corvid_panel_prepare() };
}

fn view(window: &Window) -> Result<*mut c_void> {
    let handle = HasWindowHandle::window_handle(window)
        .map_err(|error| anyhow::anyhow!("reading AppKit window handle: {error}"))?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        bail!("Agent Deck requires an AppKit panel");
    };
    Ok(handle.ns_view.as_ptr())
}

pub(super) fn hide(window: &Window) -> Result<()> {
    unsafe { corvid_panel_hide(view(window)?) };
    Ok(())
}

pub(super) fn update(
    window: &Window,
    size: Size<Pixels>,
    presentation: Option<Presentation>,
) -> Result<()> {
    let presentation = match presentation {
        None => 0,
        Some(Presentation::Open) => 1,
        Some(Presentation::Toggle) => 2,
        Some(Presentation::Hide) => 3,
    };
    // Success means the operation was queued. AppKit can invoke GPUI callbacks
    // while changing the frame and key window, so the bridge defers both.
    unsafe {
        corvid_panel_update(
            view(window)?,
            f32::from(size.width).into(),
            f32::from(size.height).into(),
            presentation,
        );
    }
    Ok(())
}
