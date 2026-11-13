use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering};

use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, PostMessageW, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos, WM_CLOSE,
    WM_PAINT,
};

const ID: &str = "agent-deck-native-window";

pub(super) struct NativeWindow(Arc<AtomicIsize>);

impl NativeWindow {
    pub fn bind(creation: &eframe::CreationContext<'_>) -> Option<Self> {
        let handle = creation.window_handle().ok()?;
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return None;
        };
        let window = Arc::new(AtomicIsize::new(handle.hwnd.get()));
        creation.egui_ctx.data_mut(|data| {
            data.insert_temp(egui::Id::new(ID), window.clone());
        });
        Some(Self(window))
    }

    pub fn drag(&self, ctx: &egui::Context) {
        let handle = self.0.load(Ordering::Acquire);
        if handle == 0 {
            return;
        }
        let Some(origin) = ctx.input(|input| input.pointer.press_origin()) else {
            return;
        };
        let scale = ctx.pixels_per_point();
        let mut point = POINT::default();
        // Keep the grabbed point under the cursor using native screen pixels.
        // Local pointer deltas change when the borderless window moves, so they
        // cannot be added to its position on every frame.
        unsafe {
            if GetCursorPos(&mut point) == 0 {
                return;
            }
            SetWindowPos(
                handle as _,
                std::ptr::null_mut(),
                point.x - (origin.x * scale).round() as i32,
                point.y - (origin.y * scale).round() as i32,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
            );
        }
    }
}

impl Drop for NativeWindow {
    fn drop(&mut self) {
        // The UI owns this guard. Stop background callbacks from targeting the
        // native handle once the app is being dropped.
        self.0.store(0, Ordering::Release);
    }
}

pub(super) fn wake_for_quit(ctx: &egui::Context) {
    let window = ctx.data(|data| data.get_temp::<Arc<AtomicIsize>>(egui::Id::new(ID)));
    let Some(window) = window else { return };
    let handle = window.load(Ordering::Acquire);
    if handle == 0 {
        return;
    }
    // Winit 0.30 uses RedrawWindow for request_redraw, which does not wake a
    // hidden HWND. Post the close event followed by a paint message so eframe
    // processes the latched Quit and runs its normal shutdown without showing
    // or focusing the window. Posting avoids blocking the tray thread.
    unsafe {
        PostMessageW(handle as _, WM_CLOSE, 0, 0);
        PostMessageW(handle as _, WM_PAINT, 0, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, IsWindowVisible, MSG, PM_REMOVE, PeekMessageW,
    };
    use windows_sys::w;

    #[test]
    fn quit_wakes_a_hidden_window_without_showing_it_and_retires_the_handle() {
        // A system STATIC window supplies an isolated, hidden HWND on this test
        // thread. No desktop session, tray icon, or notification is created.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                w!("STATIC"),
                w!("Agent Deck quit test"),
                0,
                0,
                0,
                1,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(!hwnd.is_null());
        let ctx = egui::Context::default();
        let handle = Arc::new(AtomicIsize::new(hwnd as isize));
        let guard = NativeWindow(handle.clone());
        ctx.data_mut(|data| data.insert_temp(egui::Id::new(ID), handle));
        wake_for_quit(&ctx);
        let mut message = MSG::default();
        let close = unsafe { PeekMessageW(&mut message, hwnd, WM_CLOSE, WM_CLOSE, PM_REMOVE) };
        let paint = unsafe { PeekMessageW(&mut message, hwnd, WM_PAINT, WM_PAINT, PM_REMOVE) };
        let visible = unsafe { IsWindowVisible(hwnd) };
        drop(guard);
        wake_for_quit(&ctx);
        let retired_close =
            unsafe { PeekMessageW(&mut message, hwnd, WM_CLOSE, WM_CLOSE, PM_REMOVE) };
        unsafe { DestroyWindow(hwnd) };
        assert_ne!(close, 0);
        assert_ne!(paint, 0);
        assert_eq!(visible, 0);
        assert_eq!(retired_close, 0);
    }
}
