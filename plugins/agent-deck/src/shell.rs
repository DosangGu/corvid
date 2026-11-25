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
    let mut options = WindowOptions {
        app_id: Some("corvid-agent-deck".into()),
        titlebar: Some(TitlebarOptions {
            title: Some("Corvid Agent Deck".into()),
            appears_transparent: true,
            ..Default::default()
        }),
        window_bounds: Some(WindowBounds::centered(
            None,
            window::layout_size(window.layout()),
            cx,
        )),
        focus: false,
        show: cfg!(target_os = "linux"),
        window_min_size: Some(size(px(204.), px(140.))),
        ..Default::default()
    };
    if cfg!(target_os = "macos") {
        // A card click needs to activate Deck before it can yield to Desktop.
        // GPUI's PopUp uses a nonactivating AppKit panel.
        options.kind = gpui::WindowKind::Floating;
        options.window_background = gpui::WindowBackgroundAppearance::Transparent;
        options.is_movable = false;
        options.is_resizable = false;
        options.is_minimizable = false;
    }
    options
}
