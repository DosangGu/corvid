mod hooks;

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use corvid_protocol::snapshot::SynchronizedSnapshot;
use corvid_protocol::wire::snapshot_entity::Body as EntityBody;
use corvid_protocol::wire::{
    AgentBadge, AgentCollection, AgentEndReason, AgentInfo, AgentProcessState, AgentProvider,
    LayoutRecovery, PersistenceState,
};
use tokio::signal::unix::{SignalKind, signal};

use crate::hooks::HookProvider;

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
    /// Print an additive provider hook configuration for explicit installation.
    Hooks {
        #[arg(value_enum)]
        provider: HookProvider,
    },
    /// Submit one provider observation from stdin.
    Observe {
        #[arg(long, value_enum)]
        provider: HookProvider,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ControlRequest {
    Host,
    Ping,
    Status,
    Workspaces,
    Agents,
    Clients,
    Snapshot,
    Watch,
}

#[derive(Clone, Copy, ValueEnum)]
enum ManagedAgent {
    Claude,
}

fn main() -> ExitCode {
    let started = Instant::now();
    let options = Options::parse();
    let result = match options.command {
        Command::Observe { provider } => {
            hooks::observe(provider, options.runtime_dir, started);
            Ok(0)
        }
        Command::Hooks { provider } => hooks::settings(provider).map(|settings| {
            println!("{settings:#}");
            0
        }),
        _ => execute(options),
    };
    match result {
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
        Command::Hooks { .. } | Command::Observe { .. } => unreachable!("handled before runtime"),
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
                    print_agent(agent);
                }
                Ok(0)
            }
            ControlRequest::Clients => {
                for client in corvid_client::clients(&runtime).await?.clients {
                    print!(
                        "{} generation={}",
                        client.client_id, client.connection_generation
                    );
                    if let Some(terminal) = client.terminal_id {
                        print!(" terminal={terminal}");
                    }
                    if let Some(revision) = client.view_revision {
                        print!(" view_revision={revision}");
                    }
                    for lease in client.writer_leases {
                        print!(" writer={}/{}", lease.workspace_id, lease.generation);
                    }
                    println!();
                }
                Ok(0)
            }
            ControlRequest::Snapshot => {
                print_snapshot(&corvid_client::snapshot(&runtime).await?);
                Ok(0)
            }
            ControlRequest::Watch => {
                let mut watch = corvid_client::watch_snapshot(&runtime).await?;
                print_snapshot(watch.state());
                loop {
                    watch.next_change().await?;
                    print_snapshot(watch.state());
                }
            }
        },
    }
}

fn print_snapshot(cache: &SynchronizedSnapshot) {
    let mut workspaces = 0;
    let mut terminals = 0;
    let mut clients = 0;
    let mut agents = 0;
    let mut requests = 0;
    let mut attention = 0;
    for entry in cache.snapshot().entities() {
        match &entry.body {
            Some(EntityBody::Workspace(_)) => workspaces += 1,
            Some(EntityBody::Terminal(_)) => terminals += 1,
            Some(EntityBody::Client(_)) => clients += 1,
            Some(EntityBody::Agent(_)) => agents += 1,
            Some(EntityBody::Request(_)) => requests += 1,
            Some(EntityBody::Attention(_)) => attention += 1,
            None => {}
        }
    }
    println!(
        "host_id={} server_epoch={} sequence={} workspaces={workspaces} terminals={terminals} clients={clients} agents={agents} requests={requests} attention={attention}",
        cache.session().host_id,
        cache.session().server_epoch,
        cache.snapshot().sequence(),
    );
}

fn print_agent(agent: AgentInfo) {
    let provider = AgentProvider::try_from(agent.provider).unwrap_or(AgentProvider::Unspecified);
    let process =
        AgentProcessState::try_from(agent.process).unwrap_or(AgentProcessState::Unspecified);
    let badge = AgentBadge::try_from(agent.badge).unwrap_or(AgentBadge::Unspecified);
    let collection =
        AgentCollection::try_from(agent.collection).unwrap_or(AgentCollection::Unspecified);
    let end = AgentEndReason::try_from(agent.end_reason).unwrap_or(AgentEndReason::Unspecified);
    print!(
        "{} terminal={} provider={provider:?} process={process:?} badge={badge:?} collection={collection:?} end={end:?} pending={} unverified={} resolved={} response={}",
        agent.agent_run_id,
        agent.terminal_id,
        agent.pending_requests,
        agent.unverified_requests,
        agent.resolved_requests,
        agent.response_observed_unix_ms.is_some(),
    );
    if let Some(process) = agent.process_identity {
        print!(" pid={}", process.pid);
    }
    if let Some(session) = agent.provider_session_id {
        print!(" session={session}");
    }
    if let Some(version) = agent.qualified_provider_version {
        print!(" version={version}");
    }
    println!();
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
