use super::*;

fn rect(left: i32, top: i32, width: i32, height: i32) -> RECT {
    RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

fn snap(frame: RECT, work_areas: &[RECT], dpi: u32) -> RECT {
    let mut targets = SnapTargets::new(frame, snap_distance(dpi));
    for work in work_areas {
        targets.consider(*work);
    }
    let mut result = frame;
    targets.apply(&mut result);
    assert_eq!(result.right - result.left, frame.right - frame.left);
    assert_eq!(result.bottom - result.top, frame.bottom - frame.top);
    result
}

fn origin(frame: RECT) -> (i32, i32) {
    (frame.left, frame.top)
}

#[test]
fn snaps_to_all_work_area_edges_and_corners() {
    let work = [rect(0, 0, 1920, 1040)];
    for (x, y, expected) in [
        (7, 300, (0, 300)),
        (1513, 300, (1520, 300)),
        (600, -7, (600, 0)),
        (600, 747, (600, 740)),
        (1513, 733, (1520, 740)),
    ] {
        assert_eq!(origin(snap(rect(x, y, 400, 300), &work, 96)), expected);
    }
}

#[test]
fn pulls_away_without_latching_and_scales_the_capture_distance() {
    let work = [rect(0, 0, 1920, 1040)];
    for (x, expected) in [(12, 0), (13, 13), (80, 80), (-13, -13), (-12, 0)] {
        assert_eq!(
            origin(snap(rect(x, 300, 400, 300), &work, 96)),
            (expected, 300)
        );
    }
    assert_eq!(origin(snap(rect(20, 300, 400, 300), &work, 192)), (0, 300));
    assert_eq!(origin(snap(rect(25, 300, 400, 300), &work, 192)), (25, 300));
    assert_eq!(snap_distance(0), 12);
    assert_eq!(snap_distance(144), 18);
}

#[test]
fn snaps_to_both_sides_of_monitor_seams_with_negative_origins() {
    let work = [rect(-1920, -200, 1920, 1040), rect(0, 0, 2560, 1400)];
    assert_eq!(
        origin(snap(rect(-407, 300, 400, 300), &work, 96)),
        (-400, 300)
    );
    assert_eq!(origin(snap(rect(7, 300, 400, 300), &work, 96)), (0, 300));
    assert_eq!(
        origin(snap(rect(-1913, -193, 400, 300), &work, 96)),
        (-1920, -200)
    );
    let stacked = [rect(0, -1080, 1920, 1080), rect(0, 0, 1920, 1040)];
    assert_eq!(
        origin(snap(rect(600, -307, 400, 300), &stacked, 96)),
        (600, -300)
    );
    assert_eq!(origin(snap(rect(600, 7, 400, 300), &stacked, 96)), (600, 0));
}

#[test]
fn ignores_remote_edge_segments_and_chooses_the_nearest_target() {
    let work = [rect(-1920, -200, 1920, 1040), rect(0, 0, 2560, 1400)];
    assert_eq!(
        origin(snap(rect(700, 537, 400, 300), &work, 96)),
        (700, 537)
    );
    let nearby = [rect(0, 0, 1920, 1040), rect(0, 8, 1920, 1040)];
    assert_eq!(origin(snap(rect(400, 7, 400, 300), &nearby, 96)), (400, 8));
}

#[test]
fn respects_side_taskbars_and_preserves_oversized_windows() {
    let side_taskbar = [rect(48, 0, 1872, 1080)];
    assert_eq!(
        origin(snap(rect(53, 300, 400, 300), &side_taskbar, 96)),
        (48, 300)
    );
    let small = [rect(0, 40, 640, 392)];
    assert_eq!(origin(snap(rect(7, 47, 800, 560), &small, 96)), (0, 40));
}

#[test]
fn translates_outer_bounds_using_the_visible_frame_without_resizing() {
    let outer = rect(1515, 738, 416, 316);
    let visible = rect(1523, 738, 400, 308);
    let mut targets = SnapTargets::new(visible, 12);
    targets.consider(rect(0, 0, 1920, 1040));
    let mut result = outer;
    targets.apply(&mut result);
    assert_eq!(origin(result), (1512, 732));
    assert_eq!(
        (result.right - result.left, result.bottom - result.top),
        (416, 316)
    );
}

#[test]
fn native_move_messages_snap_only_when_unpinned_and_keep_the_subclass_chain() {
    use windows_sys::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
    use windows_sys::Win32::UI::Shell::GetWindowSubclass;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    use windows_sys::core::w;

    struct TestWindow(HWND);
    impl Drop for TestWindow {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { DestroyWindow(self.0) };
            }
        }
    }

    // A hidden, isolated native window exercises real subclass dispatch on CI
    // without moving the user's cursor or sending input to another application.
    let mut window = TestWindow(unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("STATIC"),
            w!("Deck snap test"),
            WS_POPUP,
            100,
            100,
            200,
            100,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    });
    assert!(!window.0.is_null());
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        assert_ne!(
            GetMonitorInfoW(
                MonitorFromWindow(window.0, MONITOR_DEFAULTTONEAREST),
                &mut info
            ),
            0
        );
    }
    let proposal = rect(info.rcWork.right - 206, info.rcWork.bottom - 106, 200, 100);
    let send_move = |mut frame: RECT| {
        unsafe { SendMessageW(window.0, WM_MOVING, 0, &mut frame as *mut RECT as LPARAM) };
        frame
    };
    install(window.0, false).unwrap();
    let snapped = visible_frame(window.0, send_move(proposal));
    assert_eq!(
        (snapped.right, snapped.bottom),
        (info.rcWork.right, info.rcWork.bottom)
    );

    install(window.0, true).unwrap();
    assert_eq!(origin(send_move(proposal)), origin(proposal));
    install(window.0, false).unwrap();
    let pulled_away = rect(info.rcWork.left + 80, info.rcWork.top + 80, 200, 100);
    assert_eq!(origin(send_move(pulled_away)), origin(pulled_away));
    unsafe {
        assert_eq!(SendMessageW(window.0, WM_GETTEXTLENGTH, 0, 0), 14);
        let mut data = 0;
        assert_ne!(
            GetWindowSubclass(window.0, Some(window_proc), SUBCLASS_ID, &mut data),
            0
        );
        assert_ne!(DestroyWindow(window.0), 0);
        assert_eq!(
            GetWindowSubclass(window.0, Some(window_proc), SUBCLASS_ID, &mut data),
            0
        );
        window.0 = std::ptr::null_mut();
    }
}
