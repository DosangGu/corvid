use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use tokio::signal::unix::{SignalKind, signal};

#[derive(Parser)]
#[command(version, about = "Terminal multiplexer for interactive coding agents")]
struct Options {
    /// Override the private directory containing daemon sockets.
    #[arg(long, global = true)]
    runtime_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground.
    Daemon {
        /// Override the private directory containing daemon metadata.
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Send a read-only command or relay the control protocol over stdio.
    Control {
        #[arg(long, conflicts_with = "request")]
        stdio: bool,
        #[arg(value_enum)]
        request: Option<ControlRequest>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ControlRequest {
    Host,
    Ping,
}

fn main() -> ExitCode {
    let options = Options::parse();
    match execute(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("corvid: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn execute(options: Options) -> Result<()> {
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = executor.block_on(run(options));
    // Relay stdin uses blocking reads. A closed daemon must end the CLI even when
    // the other end of the SSH pipe remains open.
    executor.shutdown_timeout(Duration::from_millis(50));
    result
}

async fn run(options: Options) -> Result<()> {
    let runtime = runtime_path(options.runtime_dir)?;
    match options.command {
        Command::Daemon { state_dir } => {
            let state = state_path(state_dir)?;
            let mut terminate = signal(SignalKind::terminate())?;
            let mut interrupt = signal(SignalKind::interrupt())?;
            corvid_daemon::run(&runtime, &state, async move {
                tokio::select! {
                    _ = terminate.recv() => {}
                    _ = interrupt.recv() => {}
                }
            })
            .await
        }
        Command::Control { stdio: true, .. } => corvid_client::relay(&runtime).await,
        Command::Control { request, .. } => match request.unwrap_or(ControlRequest::Host) {
            ControlRequest::Host => {
                let host = corvid_client::host(&runtime).await?;
                println!(
                    "host_id={}\nserver_epoch={}",
                    host.host_id, host.server_epoch
                );
                Ok(())
            }
            ControlRequest::Ping => {
                corvid_client::ping(&runtime).await?;
                println!("pong");
                Ok(())
            }
        },
    }
}

fn runtime_path(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) =
        override_path.or_else(|| env::var_os("CORVID_RUNTIME_DIR").map(PathBuf::from))
    {
        return Ok(path);
    }
    let base = env::var_os("XDG_RUNTIME_DIR").context("set XDG_RUNTIME_DIR or --runtime-dir")?;
    ensure!(!base.is_empty(), "set XDG_RUNTIME_DIR or --runtime-dir");
    Ok(PathBuf::from(base).join("corvid"))
}

fn state_path(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path);
    }
    if let Some(base) = env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(base).join("corvid"));
    }
    let home = env::var_os("HOME").context("set XDG_STATE_HOME or --state-dir")?;
    ensure!(!home.is_empty(), "set XDG_STATE_HOME or --state-dir");
    Ok(PathBuf::from(home).join(".local/state/corvid"))
}
