#![cfg(all(windows, feature = "ui"))]

use std::path::PathBuf;
use std::process::{Child, Command};
use std::ptr::null;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use corvid_companion::{HostEndpoint, initialize};
use uuid::Uuid;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{FOLDERID_Programs, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Shell::NIN_SELECT;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

#[test]
#[ignore = "opens a temporary Deck and exercises the Windows notification area"]
fn tray_reopen_preserves_view_bounds_and_pin() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let state = directory.path().join("state");
    let controller = initialize(
        &state,
        Uuid::new_v4(),
        HostEndpoint::Ssh {
            alias: "fixture".into(),
        },
        8,
    )?;
    let shortcut = fixture_shortcut(controller)?;
    ensure!(!shortcut.exists(), "fixture shortcut already exists");
    let child = Command::new(env!("CARGO_BIN_EXE_corvid-agent-deck"))
        .arg("--managed-start")
        .arg("--state-dir")
        .arg(&state)
        .spawn()?;
    let mut deck = DeckProcess { child, shortcut };

    let tray = until("Deck tray window", || {
        find_window(deck.child.id(), Some("CorvidAgentDeckTray"), None)
    })
    .with_context(|| format!("Deck exit status: {:?}", deck.child.try_wait()))?;
    let window = until("visible Deck window", || {
        let window = find_window(deck.child.id(), None, Some("Corvid Agent Deck"))?;
        visible(window).then_some(window)
    })?;

    let mini = window_state(window)?;
    assert!(!mini.resizable && !mini.pinned);
    hide_and_reopen(window, tray, TrayEntry::Icon, mini)?;
    hide_and_reopen(window, tray, TrayEntry::Menu, mini)?;

    click_header(window, 53)?;
    until("expanded resizable Deck", || {
        window_state(window).ok().filter(|state| state.resizable)
    })?;
    let expanded = window_state(window)?;
    let [left, top, right, bottom] = expanded.bounds;
    ensure!(
        unsafe {
            SetWindowPos(
                window,
                std::ptr::null_mut(),
                left - 35,
                top - 27,
                right - left - 60,
                bottom - top - 40,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )
        } != 0,
        "moving and resizing the expanded Deck"
    );
    let moved = window_state(window)?;
    assert_ne!(moved.bounds[0..2], expanded.bounds[0..2]);
    assert_eq!(moved.bounds[2] - moved.bounds[0], right - left - 60);
    assert_eq!(moved.bounds[3] - moved.bounds[1], bottom - top - 40);
    hide_and_reopen(window, tray, TrayEntry::Icon, moved)?;
    hide_and_reopen(window, tray, TrayEntry::Menu, moved)?;

    click_header(window, 87)?;
    let pinned = until("pinned expanded Deck", || {
        window_state(window)
            .ok()
            .filter(|state| state.pinned && !state.resizable)
    })?;
    assert_eq!(pinned.bounds, moved.bounds);
    hide_and_reopen(window, tray, TrayEntry::Icon, pinned)?;
    hide_and_reopen(window, tray, TrayEntry::Menu, pinned)?;
    post(tray, WM_COMMAND, 4, 0)?;
    let exit = until("Deck exit after choosing Quit", || {
        deck.child.try_wait().ok().flatten()
    })?;
    ensure!(exit.success(), "Deck exited with {exit}");
    Ok(())
}

fn fixture_shortcut(controller: Uuid) -> Result<PathBuf> {
    let path = unsafe { SHGetKnownFolderPath(&FOLDERID_Programs, KF_FLAG_DEFAULT, None)? };
    let programs = unsafe { path.to_string() };
    unsafe { CoTaskMemFree(Some(path.0.cast())) };
    Ok(PathBuf::from(programs?).join(format!(
        "Corvid Agent Deck {}.lnk",
        &controller.simple().to_string()[..8]
    )))
}

struct DeckProcess {
    child: Child,
    shortcut: PathBuf,
}

impl Drop for DeckProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Err(error) = std::fs::remove_file(&self.shortcut)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "Could not remove fixture shortcut {}: {error}",
                self.shortcut.display()
            );
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowState {
    bounds: [i32; 4],
    pinned: bool,
    resizable: bool,
}

fn window_state(window: HWND) -> Result<WindowState> {
    let mut rect = RECT::default();
    ensure!(
        unsafe { GetWindowRect(window, &mut rect) } != 0,
        "reading Deck frame"
    );
    let style = unsafe { GetWindowLongW(window, GWL_STYLE) } as u32;
    let extended = unsafe { GetWindowLongW(window, GWL_EXSTYLE) } as u32;
    Ok(WindowState {
        bounds: [rect.left, rect.top, rect.right, rect.bottom],
        pinned: extended & WS_EX_TOPMOST != 0,
        resizable: style & WS_THICKFRAME != 0,
    })
}

fn find_window(pid: u32, class: Option<&str>, title: Option<&str>) -> Option<HWND> {
    let encode = |value: &str| value.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let class = class.map(encode);
    let title = title.map(encode);
    let mut window = std::ptr::null_mut();
    loop {
        window = unsafe {
            FindWindowExW(
                std::ptr::null_mut(),
                window,
                class.as_ref().map_or(null(), |value| value.as_ptr()),
                title.as_ref().map_or(null(), |value| value.as_ptr()),
            )
        };
        if window.is_null() {
            return None;
        }
        let mut owner = 0;
        unsafe { GetWindowThreadProcessId(window, &mut owner) };
        if owner == pid {
            return Some(window);
        }
    }
}

fn visible(window: HWND) -> bool {
    unsafe { IsWindowVisible(window) != 0 }
}

fn until<T>(description: &str, mut probe: impl FnMut() -> Option<T>) -> Result<T> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(result) = probe() {
            return Ok(result);
        }
        ensure!(Instant::now() < deadline, "waiting for {description}");
        sleep(Duration::from_millis(20));
    }
}

fn post(window: HWND, message: u32, wparam: usize, lparam: isize) -> Result<()> {
    ensure!(
        unsafe { PostMessageW(window, message, wparam, lparam) } != 0,
        "posting window message {message:#x}"
    );
    Ok(())
}

fn click_header(window: HWND, inset: i32) -> Result<()> {
    let mut client = RECT::default();
    ensure!(
        unsafe { GetClientRect(window, &mut client) } != 0,
        "reading Deck client area"
    );
    let dpi = unsafe { GetDpiForWindow(window) } as i32;
    let x = client.right - inset * dpi / 96;
    let y = 18 * dpi / 96;
    let position = ((y << 16) | x) as isize;
    post(window, WM_MOUSEMOVE, 0, position)?;
    post(window, WM_LBUTTONDOWN, 1, position)?;
    post(window, WM_LBUTTONUP, 0, position)?;
    Ok(())
}

#[derive(Debug)]
enum TrayEntry {
    Icon,
    Menu,
}

fn hide_and_reopen(
    window: HWND,
    tray: HWND,
    entry: TrayEntry,
    expected: WindowState,
) -> Result<()> {
    const TRAY_MESSAGE: u32 = WM_APP + 1;
    post(window, WM_CLOSE, 0, 0)?;
    until("hidden Deck window", || (!visible(window)).then_some(()))?;
    ensure!(
        unsafe { IsWindow(window) } != 0,
        "closing Deck destroyed its window"
    );
    match entry {
        TrayEntry::Icon => post(tray, TRAY_MESSAGE, 0, NIN_SELECT as isize)?,
        // Deliver the command TrackPopupMenu posts for its Open Deck item.
        TrayEntry::Menu => post(tray, WM_COMMAND, 1, 0)?,
    }
    until("reopened Deck window", || visible(window).then_some(()))?;
    let actual = window_state(window)?;
    assert_eq!(actual, expected, "window state changed after {entry:?}");
    eprintln!("{entry:?}: {actual:?}");
    Ok(())
}
