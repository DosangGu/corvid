use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use corvid_protocol::wire::{
    AgentBadge, AgentProcessState, AgentProvider, LayoutRecovery, PersistenceState,
};
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
    /// Attach to the live terminal, or start one explicitly.
    Attach {
        /// Start a new terminal with the user's shell.
        #[arg(long)]
        new: bool,
        /// Start the named provider; arguments after -- go to its CLI.
        #[arg(long, value_enum)]
        agent: Option<ManagedAgent>,
        /// Attach to a workspace by ID.
        #[arg(long, conflicts_with = "new_workspace")]
        workspace: Option<String>,
        /// Attach to a tab by ID within the workspace.
        #[arg(long, conflicts_with_all = ["new_workspace", "new_tab"])]
        tab: Option<String>,
        /// Create a workspace with this label and open a terminal.
        #[arg(long)]
        new_workspace: Option<String>,
        /// Create a tab with this label and open a terminal.
        #[arg(long)]
        new_tab: Option<String>,
        /// Start the shell in the last recorded directory if it still exists.
        #[arg(long, requires = "new")]
        restore_cwd: bool,
        /// Milliseconds to wait for a fragmented escape sequence.
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u64).range(1..=2000))]
        escape_delay_ms: u64,
        /// Program and arguments for an explicitly started terminal.
        #[arg(last = true)]
        argv: Vec<OsString>,
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
    Status,
    Workspaces,
    Agents,
}

#[derive(Clone, Copy, ValueEnum)]
enum ManagedAgent {
    Claude,
}

fn main() -> ExitCode {
    let options = Options::parse();
    match execute(options) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("corvid: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn execute(options: Options) -> Result<u32> {
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

async fn run(options: Options) -> Result<u32> {
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
            .map(|()| 0)
        }
        Command::Attach {
            new,
            agent,
            restore_cwd,
            escape_delay_ms,
            argv,
            workspace,
            tab,
            new_workspace,
            new_tab,
        } => {
            let launch = (new
                || agent.is_some()
                || !argv.is_empty()
                || new_workspace.is_some()
                || new_tab.is_some())
            .then_some(argv);
            let options = corvid_client::AttachOptions {
                launch,
                agent: agent.map(|agent| match agent {
                    ManagedAgent::Claude => AgentProvider::Claude,
                }),
                restore_cwd,
                escape_delay: Duration::from_millis(escape_delay_ms),
                workspace,
                tab,
                new_workspace,
                new_tab,
            };
            corvid_client::attach(&runtime, options).await
        }
        Command::Control { stdio: true, .. } => corvid_client::relay(&runtime).await.map(|()| 0),
        Command::Control { request, .. } => match request.unwrap_or(ControlRequest::Host) {
            ControlRequest::Host => {
                let host = corvid_client::host(&runtime).await?;
                println!(
                    "host_id={}\nserver_epoch={}",
                    host.host_id, host.server_epoch
                );
                Ok(0)
            }
            ControlRequest::Ping => {
                corvid_client::ping(&runtime).await?;
                println!("pong");
                Ok(0)
            }
            ControlRequest::Status => {
                let status = corvid_client::status(&runtime).await?;
                let persistence = match PersistenceState::try_from(status.persistence) {
                    Ok(PersistenceState::Saved) => "saved",
                    Ok(PersistenceState::Pending) => "pending",
                    Ok(PersistenceState::Failed) => "failed",
                    _ => bail!("unknown persistence status"),
                };
                let recovery = match LayoutRecovery::try_from(status.layout_recovery) {
                    Ok(LayoutRecovery::Normal) => "normal",
                    Ok(LayoutRecovery::Backup) => "backup",
                    Ok(LayoutRecovery::Fresh) => "fresh",
                    _ => bail!("unknown layout recovery state"),
                };
                println!(
                    "terminal_live={}\npersistence={persistence}\nlayout_recovery={recovery}",
                    status.terminal_live
                );
                Ok(0)
            }
            ControlRequest::Workspaces => {
                let list = corvid_client::workspaces(&runtime).await?;
                for workspace in list.workspaces {
                    println!("workspace {} {}", workspace.workspace_id, workspace.label);
                    for tab in workspace.tabs {
                        println!("  tab {} {}", tab.tab_id, tab.label);
                        for pane in tab.pane_ids {
                            println!("    pane {pane}");
                        }
                    }
                }
                Ok(0)
            }
            ControlRequest::Agents => {
                let list = corvid_client::agents(&runtime).await?;
                for agent in list.agents {
                    println!(
                        "{} terminal={} provider={:?} process={:?} badge={:?}",
                        agent.agent_run_id,
                        agent.terminal_id,
                        AgentProvider::try_from(agent.provider)
                            .unwrap_or(AgentProvider::Unspecified),
                        AgentProcessState::try_from(agent.process)
                            .unwrap_or(AgentProcessState::Unspecified),
                        AgentBadge::try_from(agent.badge).unwrap_or(AgentBadge::Unspecified),
                    );
                }
                Ok(0)
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
