#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod background;
mod catalog;
mod notifications;
#[cfg(windows)]
mod pairing;
mod parent;
mod platform;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use corvid_companion::Config;
use corvid_presentation::ipc::DesktopClient;
use eframe::egui;
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
    /// Start in the system tray. Requires a working tray backend.
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
    let image = image::load_from_memory(include_bytes!("../assets/app.png"))?.into_rgba8();
    let icon = egui::IconData {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    };
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([880.0, 600.0])
            .with_min_inner_size([400.0, 320.0])
            .with_icon(icon)
            .with_visible(!options.background || !cfg!(windows)),
        ..Default::default()
    };
    eframe::run_native(
        corvid_agent_deck::NAME,
        native,
        Box::new(move |creation| {
            app::configure(&creation.egui_ctx);
            let ctx = creation.egui_ctx.clone();
            let identity = Some((config.host_id, config.controller_id));
            let catalog = (state.clone(), config.host_id);
            let background =
                background::Background::start(state, address.clone(), config, ctx.clone())?;
            if !background.view.borrow().background {
                parent::show(&ctx);
            }
            let client =
                DesktopClient::start_scoped(address, identity, move || ctx.request_repaint())?;
            Ok(Box::new(app::Desktop::new(client, background, catalog)))
        }),
    )
    .map_err(|error| anyhow!("could not open Corvid Agent Deck: {error}"))
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
