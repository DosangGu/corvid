#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
mod app_icon;
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
mod appearance;
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
mod daemon_prompt;
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
mod desktop_windows;
#[cfg(target_os = "linux")]
mod linux_prompt;
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
mod local_bootstrap;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
mod remote_bootstrap;
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
use corvid_agent_deck::desktop as agent_deck;
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
mod ssh_hosts;
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
mod terminal_fonts;
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
mod terminal_paint;
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
mod terminal_session;
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
mod terminal_view;
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
mod workspace_view;

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn main() {
    eprintln!("The Corvid Desktop terminal workspace requires Windows or Linux.");
    std::process::exit(1);
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
fn main() -> anyhow::Result<std::process::ExitCode> {
    if let Some(code) = corvid_tui::run_daemon_helper() {
        return Ok(code);
    }

    #[cfg(target_os = "macos")]
    corvid_update::macos::use_registered_desktop()?;

    use std::path::PathBuf;

    use clap::Parser;
    use desktop_windows::DesktopWindows;

    #[derive(Parser)]
    #[command(about = "Attach to a Corvid multiplexer daemon")]
    struct Options {
        #[arg(long, hide = true)]
        update_ready_directory: Option<PathBuf>,
        #[arg(long, hide = true, requires = "update_ready_directory")]
        restore_window: Vec<uuid::Uuid>,
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
    #[cfg(target_os = "macos")]
    // No GPUI or Tokio threads have been started at this point.
    unsafe {
        macos::initialize_environment()
    };
    #[cfg(windows)]
    app_icon::identify()?;
    #[cfg(target_os = "linux")]
    anyhow::ensure!(
        gpui::GraphicalEnvironment::detect()
            .x11_display
            .is_some_and(|display| !display.is_empty()),
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
    let state = corvid_protocol::paths::state_directory(
        corvid_protocol::paths::StateRole::Companion,
        options.state_dir,
    )?;
    let preferences_state = state.clone();
    let agent_deck = agent_deck::Settings {
        state,
        #[cfg(windows)]
        pipe_instance: options.pipe_instance,
    };
    if let Err(error) = agent_deck.refresh_in_background() {
        eprintln!("Could not refresh Agent Deck: {error:#}");
    }
    let windows = DesktopWindows::new(agent_deck)?;
    let app_windows = windows.clone();

    #[cfg(target_os = "linux")]
    let application = gpui_platform::linux(gpui::WindowingModes::X11);
    #[cfg(not(target_os = "linux"))]
    let application = gpui_platform::application();
    #[cfg(target_os = "macos")]
    {
        let reopened = windows.clone();
        application.on_reopen(move |cx| {
            if cx.windows().is_empty() {
                reopened.new_window(cx, |error, _| {
                    eprintln!("Could not reopen Corvid: {error:#}");
                });
            }
            cx.activate(true);
        });
    }
    application.run(move |cx| {
        #[cfg(target_os = "macos")]
        macos::menus(&app_windows, cx);
        #[cfg(target_os = "linux")]
        cx.set_prompt_builder(linux_prompt::build);
        cx.set_global(appearance::Appearance::load(&preferences_state));
        cx.set_global(terminal_fonts::TerminalFonts::load(
            &preferences_state,
            cx.text_system(),
        ));
        let quitting = app_windows.clone();
        cx.on_app_quit(move |_| desktop_windows::wait_for_shutdown(quitting.start_shutdown()))
            .detach();
        app_windows.watch_installed_version(cx);
        app_windows.restore(
            destination,
            options.update_ready_directory,
            options.restore_window,
            cx,
        );
        if let Err(error) = app_windows.listen_for_deck(cx) {
            eprintln!("Could not listen for Agent Deck: {error:#}");
        }
        cx.activate(true);
    });

    let pending = windows.start_shutdown();
    if !pending.is_empty() {
        terminal_session::session_runtime()?.block_on(desktop_windows::wait_for_shutdown(pending));
    }
    Ok(std::process::ExitCode::SUCCESS)
}
