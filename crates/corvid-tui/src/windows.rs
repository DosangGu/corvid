use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use clap::{Subcommand, ValueEnum};
use corvid_client::{AttachOptions, check_tty, host, ping, runtime_path};
use uuid::Uuid;

use crate::hooks::{self, HookProvider};
use corvid_client::daemon_control::{state_path, windows as daemon};
pub(crate) use daemon::daemon_image;

#[derive(Subcommand)]
pub(super) enum Command {
    /// Run a local Corvid daemon in the foreground.
    Daemon {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long, hide = true)]
        autostarted: bool,
    },
    /// Start or reuse the local background daemon.
    EnsureDaemon {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long, hide = true)]
        ssh: bool,
    },
    /// Replace the background daemon while preserving hosted terminals.
    Upgrade {
        #[arg(long, hide = true)]
        managed_ssh_from: Option<String>,
        #[arg(long)]
        expected_host: Option<Uuid>,
        #[arg(long, hide = true)]
        ssh: bool,
    },
    /// Attach to a terminal workspace.
    Attach {
        #[command(flatten)]
        common: crate::attach_args::CommonAttachArgs,
    },
    /// Query the local daemon.
    Control {
        #[arg(long, conflicts_with = "request")]
        stdio: bool,
        #[arg(value_enum, required_unless_present = "stdio")]
        request: Option<ControlRequest>,
    },
    /// Relay the interactive protocol over SSH stdio.
    #[command(hide = true)]
    TerminalRelay,
    /// Print the provider observation hooks for explicit installation.
    Hooks {
        #[arg(value_enum)]
        provider: HookProvider,
        /// Install the Corvid observer hooks into the provider's global config.
        #[arg(long, conflicts_with = "uninstall")]
        install: bool,
        /// Remove the Corvid observer hooks from the provider's global config.
        #[arg(long, conflicts_with = "install")]
        uninstall: bool,
    },
    /// Submit a provider observation from stdin.
    #[command(hide = true)]
    Observe {
        #[arg(long, value_enum)]
        provider: HookProvider,
        #[arg(long, hide = true)]
        neutral_json: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub(super) enum ControlRequest {
    Host,
    Ping,
}

pub(super) async fn start(runtime: Option<PathBuf>) -> Result<u32> {
    start_attach(
        corvid_client::tui_runtime(runtime).await?,
        AttachOptions {
            launch_if_empty: true,
            ..AttachOptions::default()
        },
    )
    .await
}

pub(super) fn dispatch(
    command: Command,
    runtime: Option<PathBuf>,
    started: Instant,
) -> Result<u32> {
    match command {
        Command::Hooks {
            provider,
            install,
            uninstall,
        } => {
            let mut args = vec![
                "hooks",
                match provider {
                    HookProvider::Claude => "claude",
                    HookProvider::Codex => "codex",
                    HookProvider::Antigravity => "antigravity",
                },
            ];
            if install {
                args.push("--install");
            }
            if uninstall {
                args.push("--uninstall");
            }
            crate::daemon_bootstrap::forward_cli(args)
        }
        Command::Observe {
            provider,
            neutral_json,
        } => {
            hooks::observe(provider, neutral_json, false, runtime, started);
            Ok(0)
        }
        Command::Daemon {
            state_dir,
            autostarted,
        } => {
            let runtime = runtime_path(runtime)?;
            let state = state_path(state_dir)?;
            crate::daemon_bootstrap::foreground(&runtime, &state, autostarted)
        }
        Command::Control {
            stdio: false,
            request: Some(ControlRequest::Ping),
        } => crate::compio_ping(runtime),
        Command::Control { stdio: true, .. } => {
            let runtime = runtime_path(runtime)?;
            crate::run_compio(corvid_client::compio_relay(&runtime))
        }
        Command::TerminalRelay => {
            let runtime = runtime_path(runtime)?;
            crate::run_compio(corvid_client::compio_relay_tui(&runtime))
        }
        other => super::execute(move || execute(other, runtime)),
    }
}

async fn execute(command: Command, runtime: Option<PathBuf>) -> Result<u32> {
    let runtime = if matches!(command, Command::Attach { .. }) {
        corvid_client::tui_runtime(runtime).await?
    } else {
        runtime_path(runtime)?
    };
    match command {
        Command::Daemon { .. } => unreachable!("Compio daemon dispatched before the CLI executor"),
        Command::EnsureDaemon { state_dir, ssh } => {
            let state = state_path(state_dir)?;
            crate::daemon_bootstrap::ensure_with_install(|| {
                ensure_daemon(&runtime, &state, ssh, None)
            })
            .await?;
        }
        Command::Upgrade {
            managed_ssh_from: Some(managed_ssh_from),
            expected_host,
            ssh,
        } => daemon::upgrade_managed(&runtime, &managed_ssh_from, expected_host, ssh).await?,
        Command::Upgrade { .. } => {
            crate::daemon_bootstrap::require_cli_daemon().await?;
            upgrade_local_daemon(&runtime, false).await?;
        }
        Command::Attach { mut common } => {
            let workspace = super::workspace_cli::attach_target(
                &runtime,
                common.target.take(),
                common.workspace.take(),
            )
            .await?;
            let workspace_directory = if common.browse_directory {
                check_tty()?;
                crate::daemon_bootstrap::ensure_cli(&runtime).await?;
                super::directory_picker::pick(&runtime).await?
            } else {
                common.workspace_directory.take()
            };
            if common.browse_directory && workspace_directory.is_none() {
                return Ok(0);
            }
            let options = common.into_attach_options(workspace, workspace_directory);
            return super::workspace_cli::attach_existing(&runtime, options).await;
        }
        Command::Control { stdio, request } => match (stdio, request) {
            (true, _) => unreachable!("stdio relay dispatched before the CLI executor"),
            (_, Some(ControlRequest::Host)) => {
                let host = host(&runtime).await?;
                println!(
                    "host_id={} server_epoch={}",
                    host.host_id, host.server_epoch
                );
            }
            (_, Some(ControlRequest::Ping)) => {
                ping(&runtime).await?;
                println!("pong");
            }
            _ => unreachable!("Clap requires a control request or --stdio"),
        },
        Command::TerminalRelay => unreachable!("terminal relay dispatched before the CLI executor"),
        Command::Hooks { .. } | Command::Observe { .. } => unreachable!("synchronous hook command"),
    }
    Ok(0)
}

pub(super) async fn start_attach(runtime: PathBuf, mut options: AttachOptions) -> Result<u32> {
    options.remember_view = true;
    check_tty()?;
    let state = state_path(None)?;
    crate::daemon_bootstrap::ensure_with_install(|| ensure_daemon(&runtime, &state, false, None))
        .await?;
    crate::attach_monitored(&runtime, options).await
}

pub(super) async fn ensure_local_daemon(runtime: &Path) -> Result<()> {
    ensure_daemon(
        runtime,
        &state_path(None)?,
        false,
        Some("desktop-daemon.exe"),
    )
    .await
}

pub(super) async fn upgrade_local_daemon(runtime: &Path, desktop: bool) -> Result<()> {
    daemon::upgrade(runtime, desktop, crate::local_daemon_executable).await
}

pub(super) async fn desktop_handoff(runtime: &Path, host_id: Uuid, epoch: Uuid) -> Result<()> {
    daemon::desktop_handoff(runtime, host_id, epoch, crate::local_daemon_executable).await
}

async fn ensure_daemon(
    runtime: &Path,
    state: &Path,
    ssh: bool,
    desktop_image: Option<&str>,
) -> Result<()> {
    daemon::ensure(
        runtime,
        state,
        ssh,
        desktop_image,
        crate::local_daemon_executable,
    )
    .await
}
