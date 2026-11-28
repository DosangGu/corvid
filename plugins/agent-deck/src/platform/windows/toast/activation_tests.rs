use std::ptr::null;
use std::thread;
use std::time::{Duration, Instant};

use corvid_agent_deck_core::native::{
    Availability, Capabilities, NativeError, NativeServices, Status,
};
use corvid_companion::{Config, HostEndpoint};
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::*;
use crate::{app, background::Background, shell, ui_signal::UiSignal};

struct FixtureTray {
    _callbacks: Callbacks,
}

impl NativeServices for FixtureTray {
    fn update_status(&mut self, _: Status) -> Result<(), NativeError> {
        Ok(())
    }

    async fn notify(&mut self, _: Notice, _: bool) -> Result<(), NativeError> {
        Err(NativeError::Unsupported)
    }

    async fn set_login_startup(&mut self, _: bool) -> Result<(), NativeError> {
        Err(NativeError::Unsupported)
    }
}

#[test]
#[ignore = "opens an isolated Deck window to exercise WinRT activation callbacks"]
fn summary_toast_opens_settings_when_visible_and_when_hidden() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let state = directory.path().join("state");
    let host = Uuid::new_v4();
    let controller = corvid_companion::initialize(
        &state,
        host,
        HostEndpoint::Ssh {
            alias: "fixture".into(),
        },
        8,
    )?;
    let config = Config::read_if_present(&state)?.context("fixture config")?;
    let address = controller.simple().to_string();
    let signal = UiSignal::default();
    let (updates, update_rx) = corvid_async::mpsc::channel(16);
    let (callbacks_tx, callbacks_rx) = std::sync::mpsc::sync_channel(1);
    let background = Background::start(
        state.clone(),
        address.clone(),
        config,
        update_rx,
        signal.clone(),
        move |callbacks, _, _, _| {
            callbacks.capabilities.send_replace(Capabilities {
                status_icon: Availability::Ready,
                ..Default::default()
            });
            callbacks_tx.send(callbacks.clone())?;
            Ok(FixtureTray {
                _callbacks: callbacks,
            })
        },
    )?;
    let catalog = crate::catalog::Catalog::start(
        state.clone(),
        host,
        address,
        signal.clone(),
        background.desktop.clone(),
        updates,
    )?;
    let callbacks = callbacks_rx.try_recv()?;
    let preferences = shell::Window::load(&state, false)?;
    let quit = signal.clone();
    let probe = thread::spawn(move || {
        let result = check_routes(callbacks);
        quit.quit();
        result
    });
    app::run(
        app::Session {
            background,
            signal,
            catalog,
        },
        preferences,
    );
    probe.join().expect("activation probe panicked")
}

fn check_routes(callbacks: Callbacks) -> Result<()> {
    let window = wait_for("initial Deck window", || {
        deck_window().filter(|window| visible(*window))
    })?;
    let grid = window_size(window)?;
    let activate = || {
        activation_handler(callbacks.clone(), notice(Uuid::new_v4(), true))
            .Invoke(None::<&ToastNotification>, None::<&IInspectable>)
    };
    for hidden in [false, true] {
        if hidden {
            ensure!(
                unsafe { PostMessageW(window, WM_CLOSE, 0, 0) } != 0,
                "hiding fixture"
            );
            wait_for("Deck hidden to tray", || (!visible(window)).then_some(()))?;
            ensure!(unsafe { IsWindow(window) } != 0, "Deck was destroyed");
        }
        activate()?;
        wait_for("expanded notification settings", || {
            (visible(window)
                && resizable(window)
                && window_size(window).is_ok_and(|size| size.0 > grid.0))
            .then_some(())
        })?;
    }
    Ok(())
}

#[test]
fn each_toast_callback_keeps_its_original_notice() -> Result<()> {
    let _apartment = Apartment::new()?;
    let (events, mut received) = corvid_async::mpsc::channel(3);
    let callbacks = Callbacks {
        events,
        issues: corvid_async::watch::channel(None).0,
        capabilities: corvid_async::watch::channel(Default::default()).0,
        signal: UiSignal::default(),
    };
    let first = notice(Uuid::new_v4(), false);
    let second = notice(Uuid::new_v4(), false);
    let summary = notice(first.key.host, true);
    let first_callback = activation_handler(callbacks.clone(), first.clone());
    let second_callback = activation_handler(callbacks.clone(), second.clone());
    let summary_callback = activation_handler(callbacks.clone(), summary.clone());
    for (callback, expected) in [
        (second_callback, second),
        (first_callback, first),
        (summary_callback, summary),
    ] {
        callback.Invoke(None::<&ToastNotification>, None::<&IInspectable>)?;
        let Event::Activate(actual) = received.try_recv()? else {
            anyhow::bail!("expected toast activation");
        };
        assert_eq!(actual.key, expected.key);
        assert_eq!(actual.summary, expected.summary);
    }
    assert_eq!(callbacks.signal.take_presentation(), None);
    Ok(())
}

fn notice(host: Uuid, summary: bool) -> Notice {
    Notice {
        summary,
        key: corvid_agent_deck_core::notifications::Key {
            host,
            epoch: Uuid::new_v4(),
            run: Uuid::new_v4(),
            attention: Uuid::new_v4(),
        },
        title: "Fixture notification".into(),
        body: String::new(),
        created_ms: 1,
        focus_error: None,
    }
}

fn deck_window() -> Option<HWND> {
    let title = wide("Corvid Agent Deck");
    let mut previous = std::ptr::null_mut();
    loop {
        let window =
            unsafe { FindWindowExW(std::ptr::null_mut(), previous, null(), title.as_ptr()) };
        if window.is_null() {
            return None;
        }
        let mut owner = 0;
        unsafe { GetWindowThreadProcessId(window, &mut owner) };
        if owner == std::process::id() {
            return Some(window);
        }
        previous = window;
    }
}

fn visible(window: HWND) -> bool {
    unsafe { IsWindowVisible(window) != 0 }
}

fn resizable(window: HWND) -> bool {
    let style = unsafe { GetWindowLongW(window, GWL_STYLE) } as u32;
    style & WS_THICKFRAME != 0
}

fn window_size(window: HWND) -> Result<(i32, i32)> {
    let mut rect = RECT::default();
    ensure!(
        unsafe { GetWindowRect(window, &mut rect) } != 0,
        "reading Deck bounds"
    );
    Ok((rect.right - rect.left, rect.bottom - rect.top))
}

fn wait_for<T>(stage: &str, mut probe: impl FnMut() -> Option<T>) -> Result<T> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(value) = probe() {
            return Ok(value);
        }
        ensure!(Instant::now() < deadline, "waiting for {stage}");
        thread::sleep(Duration::from_millis(20));
    }
}
