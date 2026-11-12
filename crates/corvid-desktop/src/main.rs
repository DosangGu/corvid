#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(any(windows, target_os = "linux"))]
mod app_icon;
#[cfg(any(windows, target_os = "linux"))]
mod daemon_prompt;
#[cfg(any(windows, target_os = "linux"))]
mod local_bootstrap;
#[cfg(any(windows, target_os = "linux"))]
mod platform;
#[cfg(any(windows, target_os = "linux"))]
mod remote_bootstrap;
#[cfg(any(windows, target_os = "linux"))]
use corvid_agent_deck::desktop as agent_deck;
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
fn main() -> anyhow::Result<std::process::ExitCode> {
    if let Some(code) = corvid_tui::run_daemon_helper() {
        return Ok(code);
    }

    use std::path::PathBuf;

    use clap::Parser;
    use gpui::{
        App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px,
        size,
    };
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
        #[cfg(windows)]
        #[arg(long, conflicts_with = "ssh_alias")]
        wsl_distribution: Option<String>,
    }

    let options = Options::parse();
    #[cfg(windows)]
    app_icon::identify()?;
    #[cfg(target_os = "linux")]
    anyhow::ensure!(
        gpui::guess_compositor() == "X11",
        "Corvid Desktop requires an X11 or XWayland display"
    );

    let destination = options
        .ssh_alias
        .map(corvid_client::SshAlias::new)
        .transpose()?
        .map(terminal_session::Destination::Ssh);
    #[cfg(windows)]
    let destination = match options.wsl_distribution {
        Some(name) => Some(terminal_session::Destination::Wsl(
            corvid_client::WslDistribution::new(name)?,
        )),
        None => destination,
    };
    let state = options
        .state_dir
        .map_or_else(platform::default_state_dir, Ok)?;
    let agent_deck = agent_deck::Settings {
        state,
        #[cfg(windows)]
        pipe_instance: options.pipe_instance,
    };
    let lifetime = Lifetime::default();
    let app_lifetime = lifetime.clone();

    Application::new().run(move |cx: &mut App| {
        // GPUI's X11 backend stops its event loop after dropping the last window.
        #[cfg(windows)]
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1080.), px(680.)), cx);
        let options = WindowOptions {
            app_id: Some(app_icon::APP_ID.into()),
            titlebar: Some(TitlebarOptions {
                title: Some("Corvid Desktop".into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(900.), px(620.))),
            ..Default::default()
        };
        cx.open_window(options, move |window, cx| {
            #[cfg(target_os = "linux")]
            if let Err(error) = app_icon::set_window_icons() {
                eprintln!("Could not set Corvid Desktop icon: {error}");
            }
            cx.new(|cx| WorkspaceView::new(destination, app_lifetime, agent_deck, window, cx))
        })
        .expect("could not open Corvid Desktop");
        cx.activate(true);
    });

    lifetime.shutdown();
    Ok(std::process::ExitCode::SUCCESS)
}
