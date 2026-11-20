use gpui::{App, TitlebarOptions, WindowBounds, WindowOptions, px, size};

mod window;
pub(crate) use corvid_agent_deck_core::layout::{
    BAR_HEIGHT, GAP, GridLayout, KEY_HEIGHT, KEY_WIDTH, Orientation, Preset,
};
pub(crate) use window::Window;
#[cfg(target_os = "macos")]
pub(crate) use window::macos_colors;

pub(crate) fn prepare() {
    #[cfg(target_os = "macos")]
    window::prepare_macos();
}

pub(crate) fn options(window: &Window, cx: &App) -> WindowOptions {
    WindowOptions {
        app_id: Some("corvid-agent-deck".into()),
        titlebar: Some(TitlebarOptions {
            title: Some("Corvid Agent Deck".into()),
            appears_transparent: true,
            ..Default::default()
        }),
        window_bounds: Some(WindowBounds::centered(
            window::layout_size(window.layout()),
            cx,
        )),
        focus: false,
        show: cfg!(target_os = "linux"),
        window_min_size: Some(size(px(204.), px(140.))),
        #[cfg(target_os = "macos")]
        kind: gpui::WindowKind::PopUp,
        #[cfg(target_os = "macos")]
        macos_window_background: gpui::MacosWindowBackground::Transparent,
        is_movable: !cfg!(target_os = "macos"),
        is_resizable: !cfg!(target_os = "macos"),
        is_minimizable: !cfg!(target_os = "macos"),
        ..Default::default()
    }
}
