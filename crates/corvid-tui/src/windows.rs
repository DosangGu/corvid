mod daemon_process;

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::{Subcommand, ValueEnum};
use corvid_client::{AttachOptions, attach, check_tty, daemon_missing, host, ping, runtime_path};
use corvid_protocol::windows::StateDirectory;
use corvid_protocol::wire::AgentProvider;
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault};
use tokio::time::sleep;
use uuid::Uuid;
use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

use crate::hooks::{self, HookProvider};
use daemon_process::DaemonProcess;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const STARTUP_POLL: Duration = Duration::from_millis(50);

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
        #[arg(long)]
        new: bool,
        /// Observe an existing terminal without forwarding input or resizing it.
        #[arg(long, conflicts_with_all = ["new", "agent", "new_workspace", "new_tab", "argv"])]
        read_only: bool,
        /// Allow the current writer to hand off input to this read-only view.
        #[arg(long, requires = "read_only")]
        accept_writer: bool,
        /// Launch a native agent and observe its process lifecycle.
        #[arg(long, value_enum)]
        agent: Option<ManagedAgent>,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long)]
        tab: Option<String>,
        #[arg(long)]
        pane: Option<String>,
        #[arg(long, conflicts_with = "workspace")]
        new_workspace: Option<String>,
        #[arg(long, conflicts_with = "tab")]
        new_tab: Option<String>,
        #[arg(last = true)]
        argv: Vec<OsString>,
    },
    /// Query the local daemon.
    Control {
        #[arg(long, conflicts_with = "request")]
        stdio: bool,
        #[arg(value_enum, required_unless_present = "stdio")]
        request: Option<ControlRequest>,
    },
    /// Relay the interactive protocol over SSH stdio.
    TerminalRelay,
    /// Print the provider observation hooks for explicit installation.
    Hooks {
        #[arg(value_enum)]
        provider: HookProvider,
    },
    /// Submit a provider observation from stdin.
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

#[derive(Clone, Copy, ValueEnum)]
pub(super) enum ManagedAgent {
    Claude,
    Codex,
}

pub(super) async fn start(runtime: Option<PathBuf>) -> Result<u32> {
    start_attach(
        runtime_path(runtime)?,
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
        Command::Hooks { provider } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&hooks::settings(provider)?)?
            );
            Ok(0)
        }
        Command::Observe {
            provider,
            neutral_json,
        } => {
            hooks::observe(provider, neutral_json, false, runtime, started);
            Ok(0)
        }
        other => super::execute(execute(other, runtime)),
    }
}

async fn execute(command: Command, runtime: Option<PathBuf>) -> Result<u32> {
    let runtime = runtime_path(runtime)?;
    match command {
        Command::Daemon {
            state_dir,
            autostarted,
        } => {
            let state = state_path(state_dir)?;
            let mut interrupt = tokio::signal::windows::ctrl_c()?;
            corvid_daemon::run_with_options(&runtime, &state, autostarted, async {
                interrupt.recv().await;
            })
            .await?;
        }
        Command::EnsureDaemon { state_dir, ssh } => {
            ensure_daemon(&runtime, &state_path(state_dir)?, ssh, None).await?;
        }
        Command::Upgrade {
            managed_ssh_from: Some(managed_ssh_from),
            expected_host,
            ssh,
        } => upgrade_managed(&runtime, &managed_ssh_from, expected_host, ssh).await?,
        Command::Upgrade { .. } => upgrade_local_daemon(&runtime, false).await?,
        Command::Attach {
            new,
            read_only,
            accept_writer,
            agent,
            workspace,
            tab,
            pane,
            new_workspace,
            new_tab,
            argv,
        } => {
            let launch = (new || agent.is_some() || !argv.is_empty()).then_some(argv);
            let options = AttachOptions {
                launch_if_empty: launch.is_none() && !read_only,
                launch,
                read_only,
                accept_writer,
                agent: agent.map(|provider| match provider {
                    ManagedAgent::Claude => AgentProvider::Claude,
                    ManagedAgent::Codex => AgentProvider::Codex,
                }),
                workspace,
                tab,
                pane,
                new_workspace,
                new_tab,
                ..AttachOptions::default()
            };
            return start_attach(runtime, options).await;
        }
        Command::Control { stdio, request } => match (stdio, request) {
            (true, _) => corvid_client::relay(&runtime).await?,
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
        Command::TerminalRelay => corvid_client::relay_tui(&runtime).await?,
        Command::Hooks { .. } | Command::Observe { .. } => unreachable!("synchronous hook command"),
    }
    Ok(0)
}

fn state_path(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path);
    }
    Ok(local_app_data()?.join("Corvid/daemon"))
}

fn local_app_data() -> Result<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .context("LOCALAPPDATA is unavailable")
}

async fn start_attach(runtime: PathBuf, options: AttachOptions) -> Result<u32> {
    check_tty()?;
    if !daemon_running(&runtime).await? {
        ensure_daemon(&runtime, &state_path(None)?, false, None).await?;
    }
    attach(&runtime, options).await
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
    let directory = StateDirectory::create_all(runtime)?;
    let _startup = lock_startup(&directory).await?;
    let target = stop_for_upgrade(runtime, &directory, None).await?;
    let state = target.as_ref().map_or_else(
        || state_path(None),
        |target| Ok(target.state_directory.clone()),
    )?;
    start_daemon(
        runtime,
        &directory,
        &state,
        false,
        desktop.then_some("desktop-daemon.exe"),
    )
    .await?;
    verify_replacement(runtime, target.as_ref()).await
}

async fn stop_for_upgrade(
    runtime: &Path,
    directory: &StateDirectory,
    expected_host: Option<Uuid>,
) -> Result<Option<corvid_client::UpgradeTarget>> {
    let running = match daemon_running(runtime).await {
        Ok(running) => running,
        Err(error)
            if error
                .downcast_ref::<ProtocolFault>()
                .is_some_and(|fault| fault.code == "ProtocolMismatch") =>
        {
            true
        }
        Err(error) => return Err(error),
    };
    if !running {
        return Ok(None);
    }
    let previous = DaemonProcess::open(runtime).await?;
    let target = corvid_client::prepare_daemon_upgrade(runtime, expected_host).await?;
    tokio::time::timeout(COMMAND_TIMEOUT, async {
        loop {
            if previous.exit_code()?.is_none() {
                sleep(STARTUP_POLL).await;
                continue;
            }
            match directory.lock_instance() {
                Ok(lock) => {
                    drop(lock);
                    return Ok(());
                }
                Err(error) if sharing_violation(&error) => sleep(STARTUP_POLL).await,
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .context("waiting for the previous Windows daemon to stop")??;
    Ok(Some(target))
}

fn sharing_violation(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32))
}

async fn lock_startup(directory: &StateDirectory) -> Result<std::fs::File> {
    tokio::time::timeout(COMMAND_TIMEOUT, async {
        loop {
            match directory.lock_startup() {
                Ok(lock) => return Ok(lock),
                Err(error) if sharing_violation(&error) => sleep(STARTUP_POLL).await,
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .context("waiting for exclusive Windows daemon startup")?
}

async fn verify_replacement(
    runtime: &Path,
    target: Option<&corvid_client::UpgradeTarget>,
) -> Result<()> {
    if let Some(target) = target {
        let replacement = host(runtime).await?;
        ensure!(
            replacement.host_id == target.host_id
                && replacement.server_epoch != target.server_epoch,
            "replacement daemon did not reopen the previous host state"
        );
    }
    Ok(())
}

async fn upgrade_managed(
    runtime: &Path,
    previous_version: &str,
    expected_host: Option<Uuid>,
    ssh: bool,
) -> Result<()> {
    let staged = std::env::current_exe()?;
    let root = local_app_data()?.join("Corvid-SSH");
    ensure!(
        staged.parent() == Some(root.as_path())
            && staged
                .file_name()
                .is_some_and(|name| name.as_encoded_bytes().starts_with(b".corvid-")),
        "the managed upgrade must run from a staged Corvid binary"
    );
    let installation = StateDirectory::open(&root)?;
    let directory = StateDirectory::create_all(runtime)?;
    let _startup = lock_startup(&directory).await?;
    crate::verify_installed_version(&root.join("corvid.exe"), previous_version).await?;
    let image = std::fs::read(&staged).context("reading the staged Corvid binary")?;
    let previous = installation
        .read_private_file("corvid.exe", 64 * 1024 * 1024)?
        .context("the managed Corvid binary is missing")?;
    let target = stop_for_upgrade(runtime, &directory, expected_host).await?;
    if target.is_none()
        && let Some(host) = expected_host
    {
        ensure!(
            corvid_daemon::stored_host_id(&state_path(None)?)? == Some(host),
            ProtocolFault::new(
                "HostMismatch",
                "SSH alias points to a different Corvid host"
            )
        );
    }
    let state = target.as_ref().map_or_else(
        || state_path(None),
        |target| Ok(target.state_directory.clone()),
    )?;
    {
        let _instance = directory.lock_instance()?;
        installation.replace_private_file("corvid.exe", &image)?;
    }
    // The published image is used for the new daemon, while this helper remains staged.
    let executable = root.join("corvid.exe");
    let result = start_image(&executable, runtime, &state, ssh)
        .await
        .context("starting the updated managed Windows daemon");
    if result.is_err() {
        let _instance = directory.lock_instance()?;
        installation.replace_private_file("corvid.exe", &previous)?;
        drop(_instance);
        if target.is_some() {
            start_image(&executable, runtime, &state, ssh)
                .await
                .context("restarting the previous Windows daemon after the update failed")?;
        }
    }
    result?;
    verify_replacement(runtime, target.as_ref()).await
}

async fn daemon_running(runtime: &Path) -> Result<bool> {
    match ping(runtime).await {
        Ok(()) => Ok(true),
        Err(error) if daemon_missing(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

async fn ensure_daemon(
    runtime: &Path,
    state: &Path,
    ssh: bool,
    desktop_image: Option<&str>,
) -> Result<()> {
    let directory = StateDirectory::create_all(runtime)?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let _startup = loop {
        if daemon_running(runtime).await? {
            return Ok(());
        }
        match directory.lock_startup() {
            Ok(lock) => break lock,
            Err(error)
                if Instant::now() < deadline
                    && error.downcast_ref::<io::Error>().is_some_and(|error| {
                        error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32)
                    }) =>
            {
                sleep(STARTUP_POLL).await;
            }
            Err(error) => return Err(error),
        }
    };
    if daemon_running(runtime).await? {
        return Ok(());
    }
    start_daemon(runtime, &directory, state, ssh, desktop_image).await
}

async fn start_daemon(
    runtime: &Path,
    directory: &StateDirectory,
    state: &Path,
    ssh: bool,
    desktop_image: Option<&str>,
) -> Result<()> {
    let mut executable = crate::local_daemon_executable()?;
    if let Some(name) = desktop_image {
        // Keep the detached daemon from locking the GUI binary across rebuilds.
        let image = std::fs::read(&executable).context("reading the Desktop daemon image")?;
        directory.replace_private_file(name, &image)?;
        executable = runtime.join(name);
    }
    start_image(&executable, runtime, state, ssh).await
}

async fn start_image(executable: &Path, runtime: &Path, state: &Path, ssh: bool) -> Result<()> {
    let child = DaemonProcess::spawn(executable, runtime, state, ssh)
        .context("starting the Windows Corvid daemon")?;
    let result = wait_for_daemon(runtime, &child).await;
    if result.is_err() {
        child.terminate();
    }
    result
}

async fn wait_for_daemon(runtime: &Path, child: &DaemonProcess) -> Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(status) = child.exit_code()? {
            bail!(
                "Corvid daemon exited during startup ({status}); run corvid daemon in the foreground for details"
            );
        }
        if let Ok(result) = tokio::time::timeout(
            COMMAND_TIMEOUT.min(STARTUP_POLL * 4),
            daemon_running(runtime),
        )
        .await
            && result?
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("Corvid daemon startup timed out");
        }
        sleep(STARTUP_POLL).await;
    }
}
