mod companion;
#[cfg(target_os = "linux")]
mod hooks;
#[cfg(target_os = "linux")]
mod linux;

use std::future::Future;
use std::io::{self, BufRead, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use uuid::Uuid;

#[cfg(target_os = "linux")]
pub(crate) use linux::runtime_path;

#[derive(Parser)]
#[command(version, about = "Terminal multiplexer with coding-agent integrations")]
#[cfg_attr(
    target_os = "linux",
    command(after_help = "Run without a command to open the default workspace.")
)]
#[cfg_attr(windows, command(subcommand_required = true))]
struct Options {
    /// Override the private directory containing daemon sockets.
    #[cfg(target_os = "linux")]
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
}

fn main() -> ExitCode {
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
    #[cfg(target_os = "linux")]
    let daemon_runtime = options.runtime_dir;
    #[cfg(windows)]
    let daemon_runtime: Option<PathBuf> = None;

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
        None => unreachable!("Clap requires a Windows subcommand"),
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
