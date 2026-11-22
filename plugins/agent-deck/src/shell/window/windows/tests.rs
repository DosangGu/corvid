use super::*;

fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
    RECT {
        left,
        top,
        right,
        bottom,
    }
}

fn check_native_drag_snap(window: &Window, work: RECT) -> Result<()> {
    let hwnd = handle(window)?;
    let mut outer = RECT::default();
    ensure!(
        unsafe { GetWindowRect(hwnd, &mut outer) } != 0,
        "reading drag probe bounds"
    );
    let visible = snap::visible_frame(hwnd, outer);
    let x = work.right - visible.right - 6;
    let y = work.bottom - visible.bottom - 6;
    let mut proposed = rect(
        outer.left + x,
        outer.top + y,
        outer.right + x,
        outer.bottom + y,
    );
    // Dispatch through GPUI's real native window without injecting mouse input.
    unsafe { SendMessageW(hwnd, WM_MOVING, 0, &mut proposed as *mut RECT as isize) };
    let snapped = snap::visible_frame(hwnd, proposed);
    ensure!(
        snapped.right == work.right && snapped.bottom == work.bottom,
        "GPUI move proposal did not snap to the work area"
    );
    ensure!(
        proposed.right - proposed.left == outer.right - outer.left
            && proposed.bottom - proposed.top == outer.bottom - outer.top,
        "snapping resized the GPUI window"
    );
    Ok(())
}

#[test]
fn default_corner_survives_expansion_and_return_to_mini() {
    let work = rect(-1920, -120, 0, 920);
    let mini = rect(-404, 652, -12, 908);
    assert!(at_default_position(work, mini, 12));
    let expanded_origin = position(work, [800, 560], mini, true, 12);
    assert_eq!(expanded_origin, (-812, 348));
    let expanded = rect(-812, 348, -12, 908);
    assert!(at_default_position(work, expanded, 12));
    assert_eq!(position(work, [392, 256], expanded, true, 12), (-404, 652));
}

#[test]
fn moved_windows_keep_their_origin_and_only_shift_to_fit() {
    let work = rect(0, 0, 1920, 1040);
    let moved = rect(100, 80, 492, 336);
    assert!(!at_default_position(work, moved, 12));
    assert_eq!(position(work, [800, 560], moved, false, 12), (100, 80));
    assert_eq!(
        position(work, [800, 560], rect(1700, 900, 2092, 1156), false, 12),
        (1120, 480)
    );
}

#[test]
fn scaled_gap_and_small_work_areas_stay_on_screen() {
    let work = rect(0, 0, 3840, 2080);
    let origin = position(work, [784, 512], RECT::default(), true, 24);
    assert_eq!(origin, (3032, 1544));
    assert!(at_default_position(work, rect(3032, 1544, 3816, 2056), 24));
    assert_eq!(
        position(rect(0, 48, 320, 240), [392, 256], RECT::default(), true, 12),
        (0, 48)
    );
    assert!(at_default_position(
        rect(0, 48, 320, 240),
        rect(0, 48, 392, 304),
        12
    ));
}

#[test]
#[ignore = "opens an isolated GPUI window to measure actual Windows placement"]
fn native_window_tracks_the_tray_corner_after_resize() {
    use gpui::{
        AppContext, Application, Context, Render, TitlebarOptions, WindowBounds, WindowOptions,
        div, prelude::*, px, rgb, size,
    };
    use std::{cell::RefCell, rc::Rc};

    struct Probe {
        tick: usize,
        result: Rc<RefCell<Option<Result<(), String>>>>,
    }

    impl Render for Probe {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.tick += 1;
            let tick = self.tick;
            if tick <= 25 {
                window.request_animation_frame();
            }
            if [1, 5, 10, 15, 20, 25].contains(&tick) {
                cx.defer_in(window, move |view, window, cx| {
                    let outcome = (|| -> Result<()> {
                        if tick > 1 && tick < 20 {
                            let hwnd = handle(window)?;
                            let mut frame = RECT::default();
                            let mut info = MONITORINFO {
                                cbSize: size_of::<MONITORINFO>() as u32,
                                ..Default::default()
                            };
                            unsafe {
                                ensure!(
                                    GetWindowRect(hwnd, &mut frame) != 0
                                        && GetMonitorInfoW(tray_monitor(), &mut info) != 0,
                                    "reading probe bounds"
                                );
                                ensure!(
                                    MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)
                                        == tray_monitor(),
                                    "Deck started on the wrong monitor"
                                );
                                ensure!(IsWindowVisible(hwnd) != 0, "Deck did not open");
                            }
                            let gap = (12. * window.scale_factor()).round() as i32;
                            ensure!(
                                at_default_position(info.rcWork, frame, gap),
                                "tick {tick}: right/bottom gaps were {}, {} (expected {gap})",
                                info.rcWork.right - frame.right,
                                info.rcWork.bottom - frame.bottom
                            );
                            eprintln!(
                                "tick={tick} right_gap={} bottom_gap={} scale={}",
                                info.rcWork.right - frame.right,
                                info.rcWork.bottom - frame.bottom,
                                window.scale_factor()
                            );
                            check_native_drag_snap(window, info.rcWork)?;
                        }
                        match tick {
                            1 => {
                                ensure!(
                                    unsafe { IsWindowVisible(handle(window)?) } == 0,
                                    "Deck was visible before placement"
                                );
                                configure(window, false, false)?;
                                set_bounds(window, size(px(392.), px(256.)), true, cx)?;
                                show(window, true, cx)?;
                            }
                            5 => {
                                configure(window, false, true)?;
                                set_bounds(window, size(px(800.), px(560.)), false, cx)?;
                            }
                            10 => {
                                configure(window, false, false)?;
                                set_bounds(window, size(px(392.), px(256.)), false, cx)?;
                            }
                            15 => {
                                let hwnd = handle(window)?;
                                let mut info = MONITORINFO {
                                    cbSize: size_of::<MONITORINFO>() as u32,
                                    ..Default::default()
                                };
                                unsafe {
                                    ensure!(
                                        GetMonitorInfoW(tray_monitor(), &mut info) != 0,
                                        "probe monitor"
                                    );
                                    ensure!(
                                        SetWindowPos(
                                            hwnd,
                                            std::ptr::null_mut(),
                                            info.rcWork.left + 80,
                                            info.rcWork.top + 80,
                                            0,
                                            0,
                                            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE
                                        ) != 0,
                                        "moving isolated probe"
                                    );
                                }
                            }
                            20 => {
                                hide(window)?;
                                set_bounds(window, size(px(300.), px(300.)), false, cx)?;
                                show(window, true, cx)?;
                            }
                            25 => {
                                let mut frame = RECT::default();
                                let mut info = MONITORINFO {
                                    cbSize: size_of::<MONITORINFO>() as u32,
                                    ..Default::default()
                                };
                                unsafe {
                                    ensure!(
                                        IsWindowVisible(handle(window)?) != 0,
                                        "Deck did not reopen"
                                    );
                                    ensure!(
                                        GetWindowRect(handle(window)?, &mut frame) != 0
                                            && GetMonitorInfoW(tray_monitor(), &mut info) != 0,
                                        "reading moved probe"
                                    );
                                }
                                ensure!(
                                    frame.left == info.rcWork.left + 80
                                        && frame.top == info.rcWork.top + 80,
                                    "resizing moved the manual position"
                                );
                            }
                            _ => {}
                        }
                        Ok(())
                    })();
                    if tick == 25 || outcome.is_err() {
                        *view.result.borrow_mut() =
                            Some(outcome.map_err(|error| error.to_string()));
                        cx.quit();
                    }
                });
            }
            div()
                .size_full()
                .bg(rgb(0x142633))
                .text_color(rgb(0xffffff))
                .child("Corvid Deck placement test")
        }
    }

    let result = Rc::new(RefCell::new(None));
    let saved = result.clone();
    Application::new().run(move |cx| {
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("Corvid Deck placement test".into()),
                appears_transparent: true,
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::centered(size(px(392.), px(256.)), cx)),
            focus: false,
            show: false,
            ..Default::default()
        };
        cx.open_window(options, move |_, cx| cx.new(|_| Probe { tick: 0, result }))
            .unwrap();
    });
    saved
        .borrow_mut()
        .take()
        .expect("placement test did not complete")
        .unwrap();
}
