use crate::desktop_windows::DesktopWindows;
use gpui::{App, KeyBinding, Menu, MenuItem, SystemMenuType, actions};

actions!(corvid_macos, [Quit, NewWindow, CloseWindow]);

pub fn menus(windows: &DesktopWindows, cx: &mut App) {
    let manager = windows.clone();
    cx.on_action(move |_: &Quit, cx| manager.quit(cx));
    let manager = windows.clone();
    cx.on_action(move |_: &CloseWindow, cx| manager.close_active(cx));
    let manager = windows.clone();
    cx.on_action(move |_: &NewWindow, cx| {
        if let Err(error) = manager.new_window(cx) {
            eprintln!("Could not open Desktop window: {error:#}");
        }
    });
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-shift-n", NewWindow, None),
    ]);
    cx.set_menus(vec![
        Menu {
            name: "Corvid".into(),
            items: vec![
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Quit Corvid", Quit),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("New Window", NewWindow),
                MenuItem::action("Close Window", CloseWindow),
            ],
        },
    ]);
}

/// # Safety
/// Call only on the main thread, before starting any application threads.
pub unsafe fn initialize_environment() {
    let Some(shell) = corvid_protocol::macos::login_shell() else {
        return;
    };
    if let Some(path) = login_path(&shell) {
        // The caller guarantees that no concurrent environment access occurs.
        unsafe { std::env::set_var("PATH", path) };
    }
    unsafe { std::env::set_var("SHELL", shell) };
}

fn login_path(shell: &std::path::Path) -> Option<std::ffi::OsString> {
    use std::io::{Read, Seek};
    use std::os::unix::ffi::OsStringExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    // Finder does not inherit a terminal's PATH. Use the user's interactive
    // login shell, bounded in time and output; ignore profile banners.
    let mut output = tempfile::tempfile().ok()?;
    let mut child = Command::new(shell)
        .args(["-ilc", "printf '\\0CORVID_PATH\\0%s\\0' \"$PATH\""])
        .stdin(Stdio::null())
        .stdout(output.try_clone().ok()?)
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    output.rewind().ok()?;
    let mut bytes = Vec::new();
    output.take(64 * 1024).read_to_end(&mut bytes).ok()?;
    let marker = b"\0CORVID_PATH\0";
    let start = bytes
        .windows(marker.len())
        .rposition(|value| value == marker)?
        + marker.len();
    let end = bytes[start..].iter().position(|byte| *byte == 0)? + start;
    (end > start).then(|| std::ffi::OsString::from_vec(bytes[start..end].to_vec()))
}
