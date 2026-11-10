#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(any(windows, target_os = "linux"))]
mod icons;
#[cfg(any(windows, target_os = "linux"))]
mod platform;
#[cfg(any(windows, target_os = "linux"))]
mod slot_board;
#[cfg(any(windows, target_os = "linux"))]
mod ssh_bootstrap;
#[cfg(any(windows, target_os = "linux"))]
mod ssh_hosts;
#[cfg(any(windows, target_os = "linux"))]
mod terminal_paint;
#[cfg(any(windows, target_os = "linux"))]
mod terminal_session;
#[cfg(any(windows, target_os = "linux"))]
mod terminal_view;
#[cfg(any(windows, target_os = "linux"))]
mod workspace_view;

#[cfg(not(any(windows, target_os = "linux")))]
fn main() {
    eprintln!("The Corvid Desktop terminal workspace requires Windows or Linux.");
    std::process::exit(1);
}

#[cfg(any(windows, target_os = "linux"))]
fn main() -> anyhow::Result<()> {
    use std::path::PathBuf;

    use clap::Parser;
    use corvid_companion::{Config, HostEndpoint};
    use gpui::{
        App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px,
        size,
    };
    use icons::Icons;
    use workspace_view::{Lifetime, WorkspaceView};

    #[derive(Parser)]
    #[command(about = "Attach to a Corvid multiplexer daemon")]
    struct Options {
        #[cfg(windows)]
        #[arg(long, hide = true, default_value = "main")]
        pipe_instance: String,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        ssh_alias: Option<String>,
    }

    let options = Options::parse();
    #[cfg(target_os = "linux")]
    anyhow::ensure!(
        gpui::guess_compositor() == "X11",
        "Corvid Desktop requires an X11 or XWayland display"
    );

    let alias = options.ssh_alias;
    let state = options
        .state_dir
        .map_or_else(platform::default_state_dir, Ok)?;
    let configured = match Config::read_if_present(&state)? {
        Some(Config {
            endpoint: HostEndpoint::Ssh { alias },
            host_id,
            ..
        }) => Some((alias, host_id.to_string())),
        _ => None,
    };
    let slot_board = slot_board::Settings {
        state,
        #[cfg(windows)]
        pipe_instance: options.pipe_instance,
    };
    let lifetime = Lifetime::default();
    let app_lifetime = lifetime.clone();

    Application::new()
        .with_assets(Icons)
        .run(move |cx: &mut App| {
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let bounds = Bounds::centered(None, size(px(1080.), px(680.)), cx);
            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Corvid Desktop".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(900.), px(620.))),
                ..Default::default()
            };
            cx.open_window(options, move |window, cx| {
                cx.new(|cx| {
                    WorkspaceView::new(alias, configured, app_lifetime, slot_board, window, cx)
                })
            })
            .expect("could not open Corvid Desktop");
            cx.activate(true);
        });

    lifetime.shutdown();
    Ok(())
}
