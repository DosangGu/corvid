#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod background;
mod catalog;
mod delivery;
mod hosts;
mod parent;
mod platform;
mod shell;
mod ui;
mod ui_signal;

use anyhow::{Context, Result};
use clap::Parser;
use corvid_companion::monitor;
use corvid_presentation::ipc::DesktopClient;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "Corvid Agent Deck agent status display")]
struct Options {
    /// Existing companion configuration; Desktop supplies its verified host scope.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[arg(long)]
    socket_dir: Option<PathBuf>,
    #[cfg(target_os = "macos")]
    #[arg(long, hide = true, conflicts_with = "socket_dir")]
    socket_name: Option<String>,
    #[cfg(windows)]
    #[arg(long)]
    pipe_instance: Option<String>,
    /// Start without taking focus; tray mode stays hidden when its backend works.
    #[arg(long)]
    background: bool,
    #[arg(long, hide = true)]
    companion_service: bool,
    #[arg(long, hide = true)]
    managed_start: bool,
    /// Refresh a running instance in its existing scope, preserving visibility.
    #[arg(long, hide = true)]
    refresh_running: bool,
}

fn main() -> Result<()> {
    let options = Options::parse();
    let root = corvid_protocol::paths::state_directory(
        corvid_protocol::paths::StateRole::AgentDeck,
        options.state_dir,
    )?;
    let scope = monitor::discover(&root)?
        .into_iter()
        .next()
        .context("No monitored hosts yet. Connect a host in Corvid Desktop or TUI first.")?;
    let state = scope.state;
    let config = scope.config;
    #[cfg(windows)]
    let address = options.pipe_instance.unwrap_or(scope.address);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let address = options.socket_dir.unwrap_or(scope.address);
    #[cfg(target_os = "macos")]
    let address = if let Some(name) = options.socket_name {
        corvid_protocol::paths::macos_named_socket(name.as_ref())?.prepare()?
    } else {
        address
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    if options.companion_service {
        return runtime.block_on(corvid_companion::run(
            &address,
            &state,
            std::future::pending(),
        ));
    }
    if options.refresh_running {
        #[cfg(windows)]
        use corvid_agent_deck::lifecycle::ensure_current;
        #[cfg(not(windows))]
        use corvid_agent_deck::unix_lifecycle::ensure_current;
        runtime.block_on(ensure_current(
            &std::env::current_exe()?,
            &state,
            &address,
            &config,
            false,
        ))?;
        return Ok(());
    }
    #[cfg(windows)]
    if !options.managed_start
        && let Some(mut client) = runtime.block_on(corvid_agent_deck::lifecycle::ensure_current(
            &std::env::current_exe()?,
            &state,
            &address,
            &config,
            false,
        ))?
    {
        runtime.block_on(client.activate(false))?;
        return Ok(());
    }
    #[cfg(not(windows))]
    if !options.managed_start
        && let Some(mut client) =
            runtime.block_on(corvid_agent_deck::unix_lifecycle::ensure_current(
                &std::env::current_exe()?,
                &state,
                &address,
                &config,
                false,
            ))?
    {
        if !options.background {
            runtime.block_on(client.activate(false))?;
        }
        return Ok(());
    }
    drop(runtime);
    #[cfg(windows)]
    platform::identify(config.controller_id)?;
    #[cfg(target_os = "linux")]
    anyhow::ensure!(
        gpui::guess_compositor() == "X11",
        "Agent Deck requires an X11 or XWayland display"
    );
    let signal = ui_signal::UiSignal::default();
    let preferences = shell::Window::load(&state, options.background)?;
    let identity = Some((config.host_id, config.controller_id));
    let catalog = (state.clone(), config.host_id);
    let background = background::Background::start(
        state,
        address.clone(),
        config,
        signal.clone(),
        platform::start,
    )?;
    let notify = signal.clone();
    let client = DesktopClient::start_scoped(address, identity, move || notify.notify())?;
    app::run(
        app::Session {
            client,
            background,
            signal,
            catalog,
        },
        preferences,
    );
    Ok(())
}
