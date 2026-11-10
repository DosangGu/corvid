#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
#[cfg(windows)]
mod pairing;

use anyhow::{Result, anyhow};
use clap::Parser;
use corvid_presentation::ipc::DesktopClient;
use eframe::egui;
use uuid::Uuid;

use app::Desktop;

#[derive(Parser)]
#[command(version, about = "Corvid Slot Board")]
struct Options {
    /// Override the directory containing the local companion socket.
    #[cfg(target_os = "linux")]
    #[arg(long)]
    socket_dir: Option<std::path::PathBuf>,
    /// Choose the same pipe instance used by the local companion.
    #[cfg(windows)]
    #[arg(long, default_value = "main")]
    pipe_instance: String,
    #[arg(long, requires = "controller_id")]
    host_id: Option<Uuid>,
    #[arg(long, requires = "host_id")]
    controller_id: Option<Uuid>,
}

fn main() -> Result<()> {
    let options = Options::parse();
    #[cfg(target_os = "linux")]
    let address = options
        .socket_dir
        .or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR")
                .filter(|path| !path.is_empty())
                .map(|base| std::path::PathBuf::from(base).join("corvid-companion"))
        })
        .ok_or_else(|| anyhow!("set XDG_RUNTIME_DIR or pass --socket-dir"))?;
    #[cfg(windows)]
    let address = options.pipe_instance;
    let identity = options.host_id.zip(options.controller_id);

    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([920.0, 650.0])
            .with_min_inner_size([400.0, 320.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Corvid Slot Board",
        native,
        Box::new(move |creation| {
            app::configure(&creation.egui_ctx);
            let ctx = creation.egui_ctx.clone();
            let client = DesktopClient::start_scoped(address, identity, move || {
                ctx.request_repaint();
            })?;
            Ok(Box::new(Desktop::new(client)))
        }),
    )
    .map_err(|error| anyhow!("could not open Corvid Slot Board: {error}"))
}
