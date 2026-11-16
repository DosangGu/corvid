use anyhow::{Result, ensure};
use gpui::{Pixels, Size, Window};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MonitorFromPoint, MonitorFromRect, MonitorFromWindow,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows_sys::Win32::UI::Shell::{ABM_GETTASKBARPOS, APPBARDATA, SHAppBarMessage};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

fn handle(window: &Window) -> Result<HWND> {
    let raw = HasWindowHandle::window_handle(window)
        .map_err(|error| anyhow::anyhow!("accessing Agent Deck window: {error}"))?;
    let RawWindowHandle::Win32(raw) = raw.as_raw() else {
        anyhow::bail!("Agent Deck needs a Win32 window");
    };
    Ok(raw.hwnd.get() as HWND)
}

pub fn start_move(window: &Window) -> Result<()> {
    let hwnd = handle(window)?;
    // GPUI captures client mouse presses and its focusable root prevents
    // default non-client dragging. Queue the native move after dispatch,
    // so the modal move loop cannot re-enter a borrowed GPUI view.
    unsafe {
        ReleaseCapture();
        ensure!(
            PostMessageW(hwnd, WM_SYSCOMMAND, (SC_MOVE | HTCAPTION) as usize, 0) != 0,
            "starting Agent Deck window move"
        );
    }
    Ok(())
}

pub fn visible(window: &Window, visible: bool) -> Result<()> {
    unsafe {
        ShowWindow(
            handle(window)?,
            if visible {
                if IsIconic(handle(window)?) != 0 {
                    SW_RESTORE
                } else {
                    SW_SHOWNOACTIVATE
                }
            } else {
                SW_HIDE
            },
        );
    }
    Ok(())
}

pub fn configure(window: &Window, pinned: bool, expanded: bool) -> Result<()> {
    let hwnd = handle(window)?;
    unsafe {
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32
            & !(WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MAXIMIZEBOX | WS_MINIMIZEBOX);
        let resize = if expanded && !pinned {
            WS_THICKFRAME
        } else {
            0
        };
        SetWindowLongW(hwnd, GWL_STYLE, (style | resize) as i32);
        let extended = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        SetWindowLongW(
            hwnd,
            GWL_EXSTYLE,
            ((extended | WS_EX_TOOLWINDOW) & !WS_EX_APPWINDOW) as i32,
        );
        ensure!(
            SetWindowPos(
                hwnd,
                if pinned { HWND_TOPMOST } else { HWND_NOTOPMOST },
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED
            ) != 0,
            "updating Agent Deck window style"
        );
    }
    Ok(())
}

pub fn set_bounds(window: &mut Window, size: Size<Pixels>, initial: bool) -> Result<()> {
    let hwnd = handle(window)?;
    let scale = window.scale_factor();
    unsafe {
        let monitor = if initial {
            tray_monitor()
        } else {
            MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)
        };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let mut rect = RECT::default();
        let mut client = RECT::default();
        ensure!(
            GetMonitorInfoW(monitor, &mut info) != 0
                && GetWindowRect(hwnd, &mut rect) != 0
                && GetClientRect(hwnd, &mut client) != 0,
            "reading Agent Deck monitor bounds"
        );
        // Use the requested dimensions for clamping, since GPUI queues
        // the resize on the foreground executor.
        let width = (f32::from(size.width) * scale).round() as i32
            + (rect.right - rect.left - client.right);
        let height = (f32::from(size.height) * scale).round() as i32
            + (rect.bottom - rect.top - client.bottom);
        let gap = (12. * scale).round() as i32;
        let anchored = initial || at_default_position(info.rcWork, rect, gap);
        let (x, y) = position(info.rcWork, [width, height], rect, anchored, gap);
        ensure!(
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER
            ) != 0,
            "placing Agent Deck window"
        );
        // GPUI applies resize asynchronously. Align the final outer frame,
        // including native borders and any DPI change caused by the move.
        if anchored {
            window.on_next_frame(move |window, _| {
                if let Err(error) = align_after_resize(window, initial) {
                    eprintln!("Could not align Agent Deck: {error:#}");
                }
            });
        }
    }
    if window.viewport_size() != size {
        window.resize(size);
    }
    Ok(())
}

fn tray_monitor() -> HMONITOR {
    let mut taskbar = APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        ..Default::default()
    };
    // The shell's taskbar owns the notification area. Cursor position is not
    // a stable default, particularly when Deck starts in the background.
    unsafe {
        if SHAppBarMessage(ABM_GETTASKBARPOS, &mut taskbar) != 0 {
            MonitorFromRect(&taskbar.rc, MONITOR_DEFAULTTOPRIMARY)
        } else {
            MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY)
        }
    }
}

fn at_default_position(work: RECT, current: RECT, gap: i32) -> bool {
    let (x, y) = super::super::bottom_right(
        [work.left, work.top, work.right, work.bottom],
        [current.right - current.left, current.bottom - current.top],
        gap,
    );
    (current.left - x).abs() <= 1 && (current.top - y).abs() <= 1
}

fn position(work: RECT, size: [i32; 2], current: RECT, anchored: bool, gap: i32) -> (i32, i32) {
    let work = [work.left, work.top, work.right, work.bottom];
    if anchored {
        super::super::bottom_right(work, size, gap)
    } else {
        super::super::window_position(work, size, false, [current.left, current.top])
    }
}

fn align_after_resize(window: &Window, initial: bool) -> Result<()> {
    let hwnd = handle(window)?;
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let mut rect = RECT::default();
    unsafe {
        let monitor = if initial {
            tray_monitor()
        } else {
            MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)
        };
        ensure!(
            GetMonitorInfoW(monitor, &mut info) != 0 && GetWindowRect(hwnd, &mut rect) != 0,
            "reading resized Agent Deck bounds"
        );
        let gap = (12. * window.scale_factor()).round() as i32;
        let (x, y) = position(
            info.rcWork,
            [rect.right - rect.left, rect.bottom - rect.top],
            rect,
            true,
            gap,
        );
        ensure!(
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER
            ) != 0,
            "aligning Agent Deck above the taskbar"
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "windows/tests.rs"]
mod tests;
