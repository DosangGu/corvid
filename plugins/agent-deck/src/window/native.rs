#[cfg(target_os = "linux")]
#[path = "x11.rs"]
mod implementation;

#[cfg(windows)]
mod implementation {
    use anyhow::{Result, ensure};
    use gpui::{Pixels, Size, Window};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, MonitorFromWindow,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    fn handle(window: &Window) -> Result<HWND> {
        let raw = HasWindowHandle::window_handle(window)
            .map_err(|error| anyhow::anyhow!("accessing Agent Deck window: {error}"))?;
        let RawWindowHandle::Win32(raw) = raw.as_raw() else {
            anyhow::bail!("Agent Deck needs a Win32 window");
        };
        Ok(raw.hwnd.get() as HWND)
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
            let mut cursor = POINT::default();
            let monitor = if initial && GetCursorPos(&mut cursor) != 0 {
                MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST)
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
            let (x, y) = super::super::window_position(
                [
                    info.rcWork.left,
                    info.rcWork.top,
                    info.rcWork.right,
                    info.rcWork.bottom,
                ],
                [width, height],
                initial,
                [rect.left, rect.top],
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
                "placing Agent Deck window"
            );
        }
        if window.viewport_size() != size {
            window.resize(size);
        }
        Ok(())
    }
}

pub(super) use implementation::{configure, set_bounds, visible};
