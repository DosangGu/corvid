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
use corvid_companion::monitor::{self, Scope};
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
    let mut scope = monitor::discover(&root)?
        .into_iter()
        .next()
        .context("No monitored hosts yet. Connect a host in Corvid Desktop or TUI first.")?;
    #[cfg(windows)]
    if let Some(address) = options.pipe_instance {
        scope.address = address;
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Some(address) = options.socket_dir {
        scope.address = address;
    }
    #[cfg(target_os = "macos")]
    if let Some(name) = options.socket_name {
        scope.address = corvid_protocol::paths::macos_named_socket(name.as_ref())?.prepare()?;
    }
    let runtime = compio::runtime::Runtime::new()?;
    if options.companion_service {
        return runtime.block_on(corvid_companion::run(
            &scope.address,
            &scope.state,
            std::future::pending(),
        ));
    }
    #[cfg(not(windows))]
    if options.refresh_running {
        let profile =
            corvid_agent_deck::scope::profile_root(&scope.state, scope.config.host_id).to_owned();
        return runtime.block_on(corvid_agent_deck::unix_lifecycle::refresh_profile(
            &std::env::current_exe()?,
            &profile,
            Some(scope),
        ));
    }
    if !options.managed_start {
        scope = runtime.block_on(corvid_agent_deck::scope::shared_scope(scope))?;
    }
    let Scope {
        state,
        address,
        config,
    } = scope;
    #[cfg(windows)]
    if options.refresh_running {
        use corvid_agent_deck::lifecycle::ensure_current;
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
        if !options.background {
            runtime.block_on(client.activate(false))?;
        }
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
        gpui::GraphicalEnvironment::detect()
            .x11_display
            .is_some_and(|display| !display.is_empty()),
        "Agent Deck requires an X11 or XWayland display"
    );
    let host_id = config.host_id;
    let signal = ui_signal::UiSignal::default();
    let preferences = shell::Window::load(&state, options.background)?;
    let (updates, update_rx) = corvid_async::mpsc::channel(16);
    let background = background::Background::start(
        state.clone(),
        address.clone(),
        config,
        update_rx,
        signal.clone(),
        platform::start,
    )?;
    let catalog = catalog::Catalog::start(
        state,
        host_id,
        address,
        signal.clone(),
        background.desktop.clone(),
        updates,
    )?;
    app::run(
        app::Session {
            background,
            signal,
            catalog,
        },
        preferences,
    );
    Ok(())
}
