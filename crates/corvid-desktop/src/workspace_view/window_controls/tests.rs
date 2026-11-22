use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use gpui::{AppContext, Context, FocusHandle, Render, TitlebarOptions, WindowOptions};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    Graphics::Gdi::ClientToScreen,
    UI::{
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            HTCAPTION, HTCLIENT, IsZoomed, SC_MOVE, SMTO_ABORTIFHUNG, SendMessageTimeoutW,
            WM_MOUSEMOVE, WM_NCDESTROY, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN,
            WM_SYSCOMMAND,
        },
    },
};

use super::*;

static MOVE_REQUESTED: AtomicBool = AtomicBool::new(false);

struct Fixture(FocusHandle);

impl Render for Fixture {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().track_focus(&self.0).child(
            div()
                .flex()
                .h(px(40.))
                .pt(px(5.))
                .child(caption_spacer())
                .child(div().id("tab").w(px(120.)).h_full().cursor_pointer()),
        )
    }
}

#[test]
#[ignore = "opens an isolated native window to exercise Win32 title bar messages"]
fn caption_gestures_reach_windows_with_a_focusable_workspace() {
    let result = Rc::new(RefCell::new(None));
    let output = result.clone();
    gpui_platform::application().run(move |cx| {
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                appears_transparent: true,
                ..Default::default()
            }),
            focus: false,
            ..Default::default()
        };
        cx.open_window(options, move |window, cx| {
            window.on_next_frame(move |window, cx| {
                let handle = HasWindowHandle::window_handle(window).unwrap();
                let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                    panic!("expected a Win32 window");
                };
                let hwnd = handle.hwnd.get();
                let scale = window.scale_factor();
                assert_ne!(
                    unsafe { SetWindowSubclass(hwnd as HWND, Some(watch_move), 1, 0) },
                    0
                );
                let (send, receive) = corvid_async::oneshot::channel();
                std::thread::spawn(move || {
                    let _ = send.send(check_caption(hwnd as HWND, scale));
                });
                cx.spawn(async move |cx| {
                    *output.borrow_mut() = Some(receive.await.expect("caption probe finished"));
                    cx.update(|cx| cx.quit());
                })
                .detach();
            });
            cx.new(|cx| Fixture(cx.focus_handle()))
        })
        .unwrap();
    });
    result
        .borrow_mut()
        .take()
        .expect("caption probe ran")
        .unwrap();
}

fn check_caption(hwnd: HWND, scale: f32) -> Result<()> {
    for (x, expected) in [(80., HTCLIENT), (32., HTCAPTION)] {
        let mut point = POINT {
            x: (x * scale).round() as i32,
            y: (20. * scale).round() as i32,
        };
        send(hwnd, WM_MOUSEMOVE, 0, point)?;
        ensure!(unsafe { ClientToScreen(hwnd, &mut point) } != 0);
        ensure!(send(hwnd, WM_NCHITTEST, 0, point)? == expected as usize);

        if expected == HTCAPTION {
            send(hwnd, WM_NCLBUTTONDOWN, HTCAPTION as usize, point)?;
            ensure!(
                MOVE_REQUESTED.load(Ordering::Relaxed),
                "click did not start native window movement"
            );
            let initially_maximized = unsafe { IsZoomed(hwnd) } != 0;
            send(hwnd, WM_NCLBUTTONDBLCLK, HTCAPTION as usize, point)?;
            ensure!(
                (unsafe { IsZoomed(hwnd) } != 0) != initially_maximized,
                "double-click did not toggle maximization"
            );
        }
    }
    Ok(())
}

unsafe extern "system" fn watch_move(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _: usize,
) -> LRESULT {
    if message == WM_NCDESTROY {
        unsafe { RemoveWindowSubclass(hwnd, Some(watch_move), id) };
    } else if message == WM_SYSCOMMAND && (wparam & 0xfff0) == SC_MOVE as usize {
        // Verify the desktop received the move command without entering its modal loop.
        MOVE_REQUESTED.store(true, Ordering::Relaxed);
        return 0;
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

fn send(hwnd: HWND, message: u32, wparam: usize, point: POINT) -> Result<usize> {
    let coordinates = u32::from(point.x as u16) | (u32::from(point.y as u16) << 16);
    let mut response = 0;
    let delivered = unsafe {
        SendMessageTimeoutW(
            hwnd,
            message,
            wparam,
            coordinates as isize,
            SMTO_ABORTIFHUNG,
            2_000,
            &mut response,
        )
    };
    ensure!(
        delivered != 0,
        "Win32 message {message:#x} was not delivered"
    );
    Ok(response)
}
