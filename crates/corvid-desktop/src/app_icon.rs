use std::sync::{Arc, OnceLock};

const PNG: &[u8] = include_bytes!("../../../assets/icons/corvid.png");
pub const APP_ID: &str = "Corvid.Desktop";

pub fn image() -> Arc<gpui::Image> {
    static IMAGE: OnceLock<Arc<gpui::Image>> = OnceLock::new();
    IMAGE
        .get_or_init(|| {
            Arc::new(gpui::Image::from_bytes(
                gpui::ImageFormat::Png,
                PNG.to_vec(),
            ))
        })
        .clone()
}

#[cfg(windows)]
pub fn identify() -> anyhow::Result<()> {
    let identity: Vec<u16> = APP_ID.encode_utf16().chain(Some(0)).collect();
    // Keep the taskbar group aligned with the installed Start menu shortcut.
    let result = unsafe {
        windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(identity.as_ptr())
    };
    anyhow::ensure!(
        result >= 0,
        "setting Corvid Desktop application identity: {result:#x}"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn set_window_icons() -> anyhow::Result<()> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, PropMode};
    use x11rb::wrapper::ConnectionExt as _;

    let source = image::load_from_memory(PNG)?.into_rgba8();
    let mut pixels = Vec::new();
    // Multiple EWMH sizes cover switchers and panels, within an ordinary X11 request.
    for size in [32, 128] {
        let icon =
            image::imageops::resize(&source, size, size, image::imageops::FilterType::Lanczos3);
        pixels.extend([size, size]);
        pixels.extend(icon.pixels().map(|pixel| {
            let [r, g, b, a] = pixel.0;
            u32::from_be_bytes([a, r, g, b])
        }));
    }
    let (connection, _) = x11rb::connect(None)?;
    let property = connection
        .intern_atom(false, b"_NET_WM_ICON")?
        .reply()?
        .atom;
    for window in owned_windows(&connection)? {
        connection
            .change_property32(
                PropMode::REPLACE,
                window,
                property,
                AtomEnum::CARDINAL,
                &pixels,
            )?
            .check()?;
    }
    connection.flush()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn owned_windows(connection: &x11rb::rust_connection::RustConnection) -> anyhow::Result<Vec<u32>> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

    // GPUI 0.2.2's public X11 raw-handle accessor panics. Find only this process's
    // Corvid windows instead, including children reparented by the window manager.
    let pid_atom = connection.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;
    let mut pending: Vec<_> = connection
        .setup()
        .roots
        .iter()
        .map(|screen| screen.root)
        .collect();
    let mut visited = std::collections::HashSet::new();
    let mut owned = Vec::new();
    while let Some(window) = pending.pop() {
        if !visited.insert(window) {
            continue;
        }
        anyhow::ensure!(
            visited.len() <= 4096,
            "X11 window tree exceeds icon lookup limit"
        );
        let Ok(pid) = connection
            .get_property(false, window, pid_atom, AtomEnum::CARDINAL, 0, 1)?
            .reply()
        else {
            continue; // Another application can close a window during the lookup.
        };
        if pid.value32().and_then(|mut values| values.next()) == Some(std::process::id()) {
            let class = connection
                .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 128)?
                .reply()?;
            if class
                .value
                .split(|byte| *byte == 0)
                .any(|part| part == APP_ID.as_bytes())
            {
                owned.push(window);
                continue;
            }
        }
        if let Ok(tree) = connection.query_tree(window)?.reply() {
            pending.extend(tree.children);
        }
    }
    anyhow::ensure!(!owned.is_empty(), "Corvid X11 window is not available yet");
    Ok(owned)
}
