use std::cell::Cell;

use gpui::{Application, Render, WindowOptions};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_KEYDOWN, WM_KEYUP};

use super::*;

struct Probe {
    focus: FocusHandle,
    keys: Rc<Cell<usize>>,
}

impl Render for Probe {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this: &mut Self, _, _, _| {
                this.keys.set(this.keys.get() + 1);
            }))
    }
}

fn post_down(hwnd: HWND) {
    unsafe {
        PostMessageW(hwnd, WM_KEYDOWN, 0x28, 0x01500001);
        PostMessageW(hwnd, WM_KEYUP, 0x28, 0xc1500001u32 as isize);
    }
}

fn hwnd(window: &Window) -> HWND {
    let handle = HasWindowHandle::window_handle(window).unwrap();
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        unreachable!("Windows probe has no HWND");
    };
    handle.hwnd.get() as _
}

#[test]
#[ignore = "opens an isolated native window to exercise the Windows message loop"]
fn metadata_bursts_yield_to_win32_keys_and_frames() {
    const FRAMES: usize = 12;
    const BURST: usize = 256;
    let updates = Rc::new(Cell::new(0));
    let keys = Rc::new(Cell::new(0));
    let keys_during_burst = Rc::new(Cell::new(0));
    let view_updates = Rc::clone(&updates);
    let view_keys = Rc::clone(&keys);
    let view_keys_during_burst = Rc::clone(&keys_during_burst);
    Application::new().run(move |cx| {
        cx.open_window(WindowOptions::default(), move |window, cx| {
            let focus = cx.focus_handle();
            window.focus(&focus);
            let hwnd = hwnd(window);
            let (sender, changes) = watch::channel(0);
            let initial = sender.clone();
            let expected = Rc::clone(&view_keys);
            let probe = cx.new(|_| Probe {
                focus,
                keys: view_keys,
            });
            window
                .spawn(cx, async move |cx| {
                    watch_on_frames(changes, cx, move |value, window, cx| {
                        let count = view_updates.get() + 1;
                        if value != count * BURST {
                            cx.quit();
                            return false;
                        }
                        view_updates.set(count);
                        if count == FRAMES {
                            view_keys_during_burst.set(expected.get());
                            cx.quit();
                            return false;
                        }
                        if count > 1 {
                            post_down(hwnd);
                        }
                        for next in value + 1..=value + BURST {
                            sender.send_replace(next);
                        }
                        window.refresh();
                        true
                    })
                    .await;
                })
                .detach();
            // Start after the first native frame so the probe has a focus and input tree.
            window.on_next_frame(move |_, _| {
                for value in 1..=BURST {
                    initial.send_replace(value);
                }
            });
            probe
        })
        .unwrap();
        cx.spawn(async |cx| {
            cx.background_executor().timer(Duration::from_secs(5)).await;
            let _ = cx.update(|cx| cx.quit());
        })
        .detach();
    });
    assert_eq!(updates.get(), FRAMES);
    assert_eq!(keys_during_burst.get(), FRAMES - 2);
}

#[test]
#[ignore = "opens two isolated native windows to verify input during session shutdown"]
fn other_window_handles_keys_while_a_session_takes_five_seconds_to_stop() {
    let keys = Rc::new(Cell::new(0));
    let observed = Rc::new(Cell::new(None));
    let observed_at_end = Rc::clone(&observed);

    Application::new().run(move |cx| {
        let closing = cx
            .open_window(WindowOptions::default(), |_, cx| {
                cx.new(|cx| Probe {
                    focus: cx.focus_handle(),
                    keys: Rc::new(Cell::new(0)),
                })
            })
            .unwrap();

        cx.open_window(WindowOptions::default(), move |window, cx| {
            let focus = cx.focus_handle();
            window.focus(&focus);
            let hwnd = hwnd(window);
            let probe_keys = Rc::clone(&keys);
            window.on_next_frame(move |_, cx| {
                let lifetime = Lifetime::default();
                let (job, stopping, release) = Job::delayed_shutdown();
                lifetime.stage(job);
                let shutdown = lifetime.start_shutdown(|| {});
                let _ = closing.update(cx, |_, window, _| window.remove_window());
                cx.spawn(async move |cx| {
                    let executor = cx.background_executor().clone();
                    for _ in 0..5 {
                        executor.timer(Duration::from_secs(1)).await;
                        post_down(hwnd);
                    }
                    executor.timer(Duration::from_millis(200)).await;
                    let pending = matches!(shutdown.has_changed(), Ok(false));
                    let stopped_without_parking = matches!(stopping.try_recv(), Ok(false));
                    observed.set(Some((keys.get(), pending, stopped_without_parking)));
                    let _ = release.send(());
                    crate::desktop_windows::wait_for_shutdown(vec![shutdown]).await;
                    let _ = cx.update(|cx| cx.quit());
                })
                .detach();
            });
            cx.new(|_| Probe {
                focus,
                keys: probe_keys,
            })
        })
        .unwrap();
    });

    assert_eq!(observed_at_end.get(), Some((5, true, true)));
}
