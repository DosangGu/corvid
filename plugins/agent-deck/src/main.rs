#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod background;
mod catalog;
mod notifications;
mod parent;
mod platform;
mod ui_signal;
mod window;

use anyhow::{Context, Result};
use clap::Parser;
use corvid_companion::Config;
use corvid_presentation::ipc::DesktopClient;
use gpui::{App, AppContext, Application, WindowBounds, WindowOptions};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "Corvid Agent Deck background agent monitor")]
struct Options {
    /// Existing companion configuration; Desktop supplies its verified host scope.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[cfg(target_os = "linux")]
    #[arg(long)]
    socket_dir: Option<PathBuf>,
    #[cfg(windows)]
    #[arg(long, default_value = "main")]
    pipe_instance: String,
    /// Start without taking focus; tray mode stays hidden when its backend works.
    #[arg(long)]
    background: bool,
    #[arg(long, hide = true)]
    companion_service: bool,
}

fn main() -> Result<()> {
    let options = Options::parse();
    let state = options.state_dir.map_or_else(default_state, Ok)?;
    let config = Config::read_if_present(&state)?.context("Agent Deck needs a configured companion. Open it from Corvid Desktop or initialize corvid companion first.")?;
    #[cfg(windows)]
    let address = options.pipe_instance;
    #[cfg(target_os = "linux")]
    let address = options
        .socket_dir
        .or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(|base| PathBuf::from(base).join("corvid-companion"))
        })
        .context("set XDG_RUNTIME_DIR or pass --socket-dir")?;
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
    if runtime
        .block_on(corvid_agent_deck::connection::Client::connect(
            config.host_id,
            config.controller_id,
            false,
        ))
        .is_ok()
    {
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
    let preferences = window::Window::load(&state, options.background)?;
    let identity = Some((config.host_id, config.controller_id));
    let catalog = (state.clone(), config.host_id);
    let background = background::Background::start(state, address.clone(), config, signal.clone())?;
    let notify = signal.clone();
    let client = DesktopClient::start_scoped(address, identity, move || notify.notify())?;
    Application::new()
        .with_assets(app::DeckAssets)
        .run(move |cx: &mut App| {
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let options = WindowOptions {
                app_id: Some("corvid-agent-deck".into()),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Corvid Agent Deck".into()),
                    appears_transparent: true,
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::centered(preferences.layout().size(), cx)),
                focus: false,
                show: !cfg!(windows),
                window_min_size: Some(gpui::size(gpui::px(204.), gpui::px(140.))),
                ..Default::default()
            };
            cx.open_window(options, move |window, cx| {
                cx.new(|cx| {
                    app::Desktop::new(client, background, catalog, preferences, signal, window, cx)
                })
            })
            .expect("could not open Corvid Agent Deck");
        });
    Ok(())
}

fn default_state() -> Result<PathBuf> {
    #[cfg(windows)]
    return corvid_companion::launch::default_state_dir();
    #[cfg(target_os = "linux")]
    {
        let base = std::env::var_os("XDG_STATE_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })
            .context("set XDG_STATE_HOME or pass --state-dir")?;
        Ok(base.join("corvid/companion"))
    }
}
