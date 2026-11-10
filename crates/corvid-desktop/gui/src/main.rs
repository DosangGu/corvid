#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
mod terminal_paint;
#[cfg(windows)]
mod terminal_session;
#[cfg(windows)]
mod terminal_view;
#[cfg(windows)]
mod workspace_view;

#[cfg(not(windows))]
fn main() {
    eprintln!("The Corvid Desktop terminal workspace requires Windows.");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::path::PathBuf;

    use clap::Parser;
    use corvid_companion::{Config, HostEndpoint};
    use corvid_deck::icons::Icons;
    use gpui::{
        App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px,
        size,
    };
    use workspace_view::{Lifetime, WorkspaceView};

    #[derive(Parser)]
    #[command(about = "Attach to a Corvid multiplexer daemon")]
    struct Options {
        // Accepted for the existing companion launch command.
        #[arg(long = "pipe-instance", hide = true)]
        _pipe_instance: Option<String>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        ssh_alias: Option<String>,
    }

    let options = Options::parse();
    let alias = options.ssh_alias;
    let state = options
        .state_dir
        .map_or_else(corvid_companion::launch::default_state_dir, Ok)?;
    let configured = Config::read_if_present(&state)?.map(|config| {
        let HostEndpoint::Ssh { alias } = config.endpoint;
        (alias, config.host_id.to_string())
    });
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
                cx.new(|cx| WorkspaceView::new(alias, configured, app_lifetime, window, cx))
            })
            .expect("could not open Corvid Desktop");
            cx.activate(true);
        });

    lifetime.shutdown();
    Ok(())
}
