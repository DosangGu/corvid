mod companion;
#[cfg(target_os = "linux")]
mod hooks;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

use std::future::Future;
use std::io::{self, BufRead, Read};
#[cfg(any(windows, target_os = "linux"))]
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use uuid::Uuid;

#[cfg(target_os = "linux")]
pub(crate) use corvid_client::runtime_path;

#[derive(Parser)]
#[command(
    name = "corvid",
    version,
    about = "Terminal multiplexer with coding-agent integrations"
)]
#[command(after_help = "Run without a command to open the default workspace.")]
struct Options {
    /// Override the private directory containing daemon sockets.
    #[arg(long, global = true)]
    runtime_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run or use the local controller companion.
    Companion(companion::Options),
    #[cfg(target_os = "linux")]
    #[command(flatten)]
    Linux(linux::Command),
    #[cfg(windows)]
    #[command(flatten)]
    Windows(windows::Command),
}

/// Start the daemon for this runtime using the same executable as the caller.
#[cfg(any(windows, target_os = "linux"))]
pub async fn ensure_local_daemon(runtime: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::ensure(runtime).await
    }
    #[cfg(windows)]
    {
        windows::ensure_local_daemon(runtime).await
    }
}

/// Dispatch daemon subprocesses and Linux agent helpers before opening a GUI.
#[cfg(any(windows, target_os = "linux"))]
pub fn run_daemon_helper() -> Option<ExitCode> {
    #[cfg(target_os = "linux")]
    if std::env::args_os()
        .next()
        .as_deref()
        .and_then(corvid_daemon::shell_agent::invoked_provider)
        .is_some()
    {
        return Some(run());
    }

    match std::env::args_os().nth(1)?.to_str()? {
        "daemon" => Some(run()),
        #[cfg(target_os = "linux")]
        "observe" => Some(run()),
        _ => None,
    }
}

pub fn run() -> ExitCode {
    #[cfg(target_os = "linux")]
    {
        let mut args = std::env::args_os();
        if let Some(provider) = args
            .next()
            .as_deref()
            .and_then(corvid_daemon::shell_agent::invoked_provider)
        {
            return ExitCode::from(linux::shell_agent::execute(provider, args.collect()));
        }
    }
    #[cfg(target_os = "linux")]
    let started = Instant::now();
    let options = Options::parse();
    let daemon_runtime = options.runtime_dir;

    let result = match options.command {
        Some(Command::Companion(companion)) => execute(async move {
            companion::execute(companion, daemon_runtime).await?;
            Ok(0)
        }),
        #[cfg(target_os = "linux")]
        Some(Command::Linux(command)) => linux::execute(command, daemon_runtime, started),
        #[cfg(target_os = "linux")]
        None => execute(linux::start(daemon_runtime)),
        #[cfg(windows)]
        Some(Command::Windows(command)) => execute(windows::execute(command, daemon_runtime)),
        #[cfg(windows)]
        None => execute(windows::start(daemon_runtime)),
    };
    match result {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("corvid: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn execute(run: impl Future<Output = Result<u32>>) -> Result<u32> {
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = executor.block_on(run);
    // Relay stdin uses blocking reads. A closed daemon must end the CLI even when
    // the other end of the SSH pipe remains open.
    executor.shutdown_timeout(Duration::from_millis(50));
    result
}

fn read_pairing_code() -> Result<Uuid> {
    let mut input = String::new();
    io::stdin().lock().take(128).read_line(&mut input)?;
    Uuid::parse_str(input.trim()).context("invalid pairing code")
}
