#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(any(windows, target_os = "linux"))]
mod app_icon;
#[cfg(any(windows, target_os = "linux"))]
mod daemon_prompt;
#[cfg(any(windows, target_os = "linux"))]
mod desktop_windows;
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
    use desktop_windows::DesktopWindows;
    use gpui::Application;

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
    let windows = DesktopWindows::new(agent_deck);
    let app_windows = windows.clone();

    Application::new().run(move |cx| {
        let quitting = app_windows.clone();
        cx.on_app_quit(move |_| {
            quitting.shutdown();
            async {}
        })
        .detach();
        if let Err(error) = app_windows.restore(destination, cx) {
            eprintln!("Could not restore Desktop windows: {error:#}");
            app_windows.quit(cx);
        }
        cx.activate(true);
    });

    windows.shutdown();
    Ok(std::process::ExitCode::SUCCESS)
}
