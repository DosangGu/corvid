use anyhow::{Result, ensure};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt, EventMask, PropMode,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

fn connect() -> Result<(RustConnection, u32, Vec<u32>)> {
    let (connection, screen) = x11rb::connect(None)?;
    let root = connection.setup().roots[screen].root;
    let pid_atom = connection.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;
    let mut pending = vec![root];
    let mut owned = Vec::new();
    let mut visited = std::collections::HashSet::new();
    // GPUI 0.2.2's X11 raw-window accessor panics. Resolve this process's
    // application window, including window-manager reparenting.
    while let Some(window) = pending.pop() {
        if !visited.insert(window) {
            continue;
        }
        ensure!(
            visited.len() <= 4096,
            "Agent Deck X11 lookup limit exceeded"
        );
        let Ok(pid) = connection
            .get_property(false, window, pid_atom, AtomEnum::CARDINAL, 0, 1)?
            .reply()
        else {
            continue;
        };
        if pid.value32().and_then(|mut values| values.next()) == Some(std::process::id()) {
            let class = connection
                .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 128)?
                .reply()?;
            if class
                .value
                .split(|byte| *byte == 0)
                .any(|part| part == b"corvid-agent-deck")
            {
                owned.push(window);
                continue;
            }
        }
        if let Ok(tree) = connection.query_tree(window)?.reply() {
            pending.extend(tree.children);
        }
    }
    ensure!(!owned.is_empty(), "Agent Deck X11 window unavailable");
    Ok((connection, root, owned))
}

pub fn visible(_: &gpui::Window, visible: bool) -> Result<()> {
    let (connection, _, windows) = connect()?;
    for window in windows {
        if visible {
            connection.map_window(window)?.check()?;
        } else {
            connection.unmap_window(window)?.check()?;
        }
    }
    connection.flush()?;
    Ok(())
}

pub fn configure(_: &gpui::Window, topmost: bool, expanded: bool) -> Result<()> {
    let (connection, root, windows) = connect()?;
    let state = connection
        .intern_atom(false, b"_NET_WM_STATE")?
        .reply()?
        .atom;
    let above = connection
        .intern_atom(false, b"_NET_WM_STATE_ABOVE")?
        .reply()?
        .atom;
    let hints = connection
        .intern_atom(false, b"_MOTIF_WM_HINTS")?
        .reply()?
        .atom;
    let icon_atom = connection
        .intern_atom(false, b"_NET_WM_ICON")?
        .reply()?
        .atom;
    let source = image::load_from_memory(include_bytes!("../../../../assets/icons/corvid.png"))?
        .into_rgba8();
    let icon = image::imageops::resize(&source, 128, 128, image::imageops::FilterType::Lanczos3);
    let mut pixels = vec![icon.width(), icon.height()];
    pixels.extend(icon.pixels().map(|pixel| {
        let [r, g, b, a] = pixel.0;
        u32::from_be_bytes([a, r, g, b])
    }));
    for window in windows {
        connection
            .change_property32(
                PropMode::REPLACE,
                window,
                hints,
                hints,
                &[2, 0, u32::from(expanded), 0, 0],
            )?
            .check()?;
        connection
            .change_property32(
                PropMode::REPLACE,
                window,
                icon_atom,
                AtomEnum::CARDINAL,
                &pixels,
            )?
            .check()?;
        let event =
            ClientMessageEvent::new(32, window, state, [u32::from(topmost), above, 0, 1, 0]);
        connection
            .send_event(
                false,
                root,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                event,
            )?
            .check()?;
    }
    connection.flush()?;
    Ok(())
}

pub fn place(_: &gpui::Window, fixed: bool, _: bool) -> Result<()> {
    if !fixed {
        return Ok(());
    }
    let (connection, root, windows) = connect()?;
    let screen = connection.get_geometry(root)?.reply()?;
    for window in windows {
        let geometry = connection.get_geometry(window)?.reply()?;
        let x = (i32::from(screen.width) - i32::from(geometry.width)).max(0) / 2;
        let y = (i32::from(screen.height) - i32::from(geometry.height)).max(0) / 2;
        connection
            .configure_window(window, &ConfigureWindowAux::new().x(x).y(y))?
            .check()?;
    }
    connection.flush()?;
    Ok(())
}
