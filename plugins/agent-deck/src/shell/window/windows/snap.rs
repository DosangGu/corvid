use anyhow::{Result, ensure};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowRect, WM_MOVING, WM_NCDESTROY};

const SUBCLASS_ID: usize = 1;
const SNAP_DISTANCE: i32 = 12;

pub(super) fn install(hwnd: HWND, pinned: bool) -> Result<()> {
    // configure runs on the window's owning thread. Reinstalling this pair
    // updates the pin flag without adding another callback or owning heap data.
    ensure!(
        unsafe { SetWindowSubclass(hwnd, Some(window_proc), SUBCLASS_ID, usize::from(pinned)) }
            != 0,
        "installing Agent Deck edge snapping"
    );
    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass_id: usize,
    pinned: usize,
) -> LRESULT {
    if message == WM_NCDESTROY {
        unsafe { RemoveWindowSubclass(hwnd, Some(window_proc), subclass_id) };
    } else if message == WM_MOVING && pinned == 0 && lparam != 0 {
        // WM_MOVING supplies the proposed screen rectangle during the native
        // move loop. Adjust that proposal, never move the window recursively.
        let proposed = unsafe { &mut *(lparam as *mut RECT) };
        let frame = visible_frame(hwnd, *proposed);
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let mut targets = SnapTargets::new(frame, snap_distance(dpi));
        let enumerated = unsafe {
            EnumDisplayMonitors(
                std::ptr::null_mut(),
                std::ptr::null(),
                Some(visit_monitor),
                &mut targets as *mut SnapTargets as LPARAM,
            )
        };
        if enumerated != 0 {
            targets.apply(proposed);
            return 1;
        }
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

fn snap_distance(dpi: u32) -> i32 {
    let dpi = if dpi == 0 { 96 } else { dpi };
    ((dpi * SNAP_DISTANCE as u32 + 48) / 96) as i32
}

pub(super) fn visible_frame(hwnd: HWND, proposed: RECT) -> RECT {
    let mut outer = RECT::default();
    let mut visible = RECT::default();
    unsafe {
        if GetWindowRect(hwnd, &mut outer) == 0
            || DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                &mut visible as *mut RECT as *mut _,
                size_of::<RECT>() as u32,
            ) < 0
        {
            return proposed;
        }
    }
    // DWM can return an empty frame for a hidden window. GPUI is DPI-aware,
    // so valid DWM and window rectangles are both physical screen coordinates.
    if visible.right <= visible.left
        || visible.bottom <= visible.top
        || visible.left < outer.left
        || visible.top < outer.top
        || visible.right > outer.right
        || visible.bottom > outer.bottom
    {
        return proposed;
    }
    RECT {
        left: proposed.left + visible.left - outer.left,
        top: proposed.top + visible.top - outer.top,
        right: proposed.right + visible.right - outer.right,
        bottom: proposed.bottom + visible.bottom - outer.bottom,
    }
}

unsafe extern "system" fn visit_monitor(
    monitor: HMONITOR,
    _: HDC,
    _: *mut RECT,
    data: LPARAM,
) -> i32 {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) } != 0 {
        let targets = unsafe { &mut *(data as *mut SnapTargets) };
        targets.consider(info.rcWork);
    }
    1
}

struct SnapTargets {
    frame: RECT,
    distance: i32,
    x: Option<i32>,
    y: Option<i32>,
}

impl SnapTargets {
    fn new(frame: RECT, distance: i32) -> Self {
        Self {
            frame,
            distance,
            x: None,
            y: None,
        }
    }

    fn consider(&mut self, work: RECT) {
        // Every monitor contributes its own work-area edges, including shared
        // seams and taskbars on secondary displays. Ignore distant edge segments.
        let frame = self.frame;
        if frame.bottom >= work.top - self.distance && frame.top <= work.bottom + self.distance {
            for offset in [work.left - frame.left, work.right - frame.right] {
                choose_nearest(&mut self.x, offset, self.distance);
            }
        }
        if frame.right >= work.left - self.distance && frame.left <= work.right + self.distance {
            for offset in [work.top - frame.top, work.bottom - frame.bottom] {
                choose_nearest(&mut self.y, offset, self.distance);
            }
        }
    }

    fn apply(&self, proposed: &mut RECT) {
        let x = self.x.unwrap_or_default();
        let y = self.y.unwrap_or_default();
        proposed.left += x;
        proposed.right += x;
        proposed.top += y;
        proposed.bottom += y;
    }
}

fn choose_nearest(best: &mut Option<i32>, offset: i32, distance: i32) {
    if offset.abs() <= distance && best.is_none_or(|current| offset.abs() < current.abs()) {
        *best = Some(offset);
    }
}

#[cfg(test)]
#[path = "snap_tests.rs"]
mod tests;
