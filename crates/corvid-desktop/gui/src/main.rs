#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(any(windows, test))]
mod session_layout;
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
    use corvid_deck::{deck::setup::DesktopLifetime, icons::Icons};
    use gpui::{
        App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px,
        size,
    };
    use workspace_view::{AgentSetup, WorkspaceView};

    #[derive(Parser)]
    #[command(about = "Open a local Corvid terminal workspace")]
    struct Options {
        #[arg(long, default_value = "main")]
        pipe_instance: String,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        ssh_alias: Option<String>,
    }

    let options = Options::parse();
    let state = options
        .state_dir
        .map_or_else(corvid_companion::launch::default_state_dir, Ok)?;
    let setup = AgentSetup {
        instance: options.pipe_instance,
        state,
        alias: options.ssh_alias,
    };
    let companion_lifetime = DesktopLifetime::default();
    let app_companion_lifetime = companion_lifetime.clone();
    let terminal_lifetime = terminal_view::Lifetime::default();
    let app_terminal_lifetime = terminal_lifetime.clone();

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
                    WorkspaceView::new(
                        setup,
                        app_companion_lifetime,
                        app_terminal_lifetime,
                        window,
                        cx,
                    )
                })
            })
            .expect("could not open Corvid Desktop");
            cx.activate(true);
        });

    companion_lifetime.shutdown();
    terminal_lifetime.shutdown();
    Ok(())
}
