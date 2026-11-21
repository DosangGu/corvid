mod companion;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod hooks;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod linux;
mod retirement;
mod update;
#[cfg(windows)]
mod windows;

use std::future::Future;
use std::io::{self, BufRead, Read};
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use uuid::Uuid;

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
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
    /// Restore the saved agent conversation in each empty pane, without attaching.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    Restore {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        pane: Option<Uuid>,
        /// Inspect saved sessions and restore outcomes without starting agents.
        #[arg(long, conflicts_with = "retry")]
        inspect: bool,
        /// Retry failed panes as well as restoring available sessions.
        #[arg(long)]
        retry: bool,
    },
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[command(hide = true)]
    DesktopHandoff {
        #[arg(long)]
        host: Uuid,
        #[arg(long)]
        epoch: Uuid,
    },
    /// Finish activation using the package surrounding this executable.
    #[command(hide = true)]
    FinishUpdate {
        #[arg(long)]
        retry: bool,
    },
    #[cfg(target_os = "macos")]
    #[command(hide = true)]
    ReplaceMacosBundle {
        #[arg(long)]
        caller: i32,
    },
    #[cfg(target_os = "macos")]
    #[command(hide = true)]
    PublishMacosDesktop {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        app: PathBuf,
    },
    /// Check for or install a GitHub release.
    Update(update::Options),
    /// Run or use the local controller companion.
    Companion(companion::Options),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[command(flatten)]
    Linux(linux::Command),
    #[cfg(windows)]
    #[command(flatten)]
    Windows(windows::Command),
}

/// Start the daemon for this runtime using the same executable as the caller.
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
pub async fn ensure_local_daemon(runtime: &Path) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        linux::ensure(runtime).await
    }
    #[cfg(windows)]
    {
        windows::ensure_local_daemon(runtime).await
    }
}

/// Start the UI-independent companion for a connection whose host was verified.
pub async fn monitor_agent_host(
    state: &Path,
    endpoint: &corvid_client::ControlEndpoint,
    host: Uuid,
    instance: &str,
) -> Result<()> {
    let scope = corvid_companion::monitor::Scope::prepare(state, endpoint, host, instance)?;
    scope.configure(endpoint)?;
    scope.ensure_running(&monitor_executable()?).await
}

pub fn monitor_executable() -> Result<PathBuf> {
    local_daemon_executable()
}

mod directory_picker;

async fn attach_monitored(runtime: &Path, options: corvid_client::AttachOptions) -> Result<u32> {
    corvid_client::check_tty()?;
    if std::env::var_os("CORVID_AGENT_MONITOR").is_some_and(|value| value == "0") {
        return corvid_client::attach(runtime, options).await;
    }
    let endpoint = corvid_client::ControlEndpoint::Local(runtime.to_owned());
    let monitor = tokio::spawn(async move {
        let host = endpoint.host().await?;
        monitor_agent_host(
            &companion::companion_state(None)?,
            &endpoint,
            Uuid::parse_str(&host.host_id)?,
            "main",
        )
        .await
    });
    let result = corvid_client::attach(runtime, options).await;
    if monitor.is_finished() {
        if let Err(error) = monitor.await.unwrap_or_else(|error| Err(error.into())) {
            // Report after restoring the TTY, never into the active terminal's render stream.
            eprintln!("corvid: agent monitor unavailable: {error:#}");
        }
    } else {
        monitor.abort();
    }
    result
}

/// Use the shared runtime payload when starting a daemon from packaged Desktop.
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
fn local_daemon_executable() -> Result<PathBuf> {
    let executable = std::env::current_exe()?;
    corvid_update::layout::local_daemon_executable(&executable)
}

/// Replace an automatically started daemon while preserving its hosted terminals.
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
pub async fn upgrade_local_daemon(runtime: &Path) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        linux::upgrade(runtime).await
    }
    #[cfg(windows)]
    {
        windows::upgrade_local_daemon(runtime, true).await
    }
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
async fn verify_installed_version(executable: &Path, previous_version: &str) -> Result<()> {
    use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault};

    let output = tokio::time::timeout(
        COMMAND_TIMEOUT,
        tokio::process::Command::new(executable)
            .arg("--version")
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    anyhow::ensure!(
        output.status.success()
            && std::str::from_utf8(&output.stdout)?.trim_end_matches(['\r', '\n'])
                == format!("corvid {previous_version}"),
        ProtocolFault::new(
            "InstallationChanged",
            "the installed Corvid version changed; reconnect and retry"
        )
    );
    Ok(())
}

/// Dispatch daemon subprocesses and Linux agent helpers before opening a GUI.
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
pub fn run_daemon_helper() -> Option<ExitCode> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if std::env::args_os()
        .next()
        .as_deref()
        .and_then(corvid_daemon::shell_agent::invoked_provider)
        .is_some()
    {
        return Some(run());
    }

    match std::env::args_os().nth(1)?.to_str()? {
        "daemon" | "terminal-host" | "desktop-handoff" | "companion" => Some(run()),
        "observe" => Some(run()),
        _ => None,
    }
}

pub fn run() -> ExitCode {
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("terminal-host")) {
        return match corvid_daemon::terminal::run_helper(std::env::args_os().skip(2)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("corvid: {error:#}");
                ExitCode::FAILURE
            }
        };
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
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
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    let started = Instant::now();
    let options = Options::parse();
    if (options.command.is_none()
        || std::env::args_os()
            .nth(1)
            .is_some_and(|arg| arg == "attach"))
        && let Ok(Some(installation)) = corvid_update::Installation::detect()
        && let Err(error) = installation.resume_retirement()
    {
        eprintln!("Could not resume Corvid cleanup: {error:#}");
    }
    let daemon_runtime = options.runtime_dir;

    let result = match options.command {
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        Some(Command::Restore {
            workspace,
            pane,
            inspect,
            retry,
        }) => execute(async move {
            use corvid_protocol::wire::{AgentRestoreMode, PaneRestoreStatus, RestoreWorkspace};
            let runtime = runtime_path(daemon_runtime)?;
            ensure_local_daemon(&runtime).await?;
            let endpoint = corvid_client::ControlEndpoint::Local(runtime);
            let mut connection = endpoint
                .interactive(vec![corvid_protocol::RESTORE_AGENTS_CAPABILITY.into()])
                .await?;
            let mode = if inspect {
                AgentRestoreMode::Inspect
            } else if retry {
                AgentRestoreMode::Retry
            } else {
                AgentRestoreMode::Resume
            };
            let result = connection
                .restore_workspace(RestoreWorkspace {
                    workspace_id: workspace.to_string(),
                    pane_id: pane.map(|pane| pane.to_string()),
                    mode: mode as i32,
                })
                .await?;
            let mut failed = false;
            for pane in result.panes {
                let status = pane.status();
                failed |= status == PaneRestoreStatus::Failed;
                let location = pane.location.context("missing restored pane")?;
                println!(
                    "{}: {:?}{}",
                    location.pane_id,
                    status,
                    pane.error.map_or_else(String::new, |error| format!(
                        " — {}: {}",
                        error.code, error.message
                    ))
                );
            }
            Ok(u32::from(failed))
        }),
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        Some(Command::DesktopHandoff { host, epoch }) => execute(async move {
            let runtime = runtime_path(daemon_runtime)?;
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            linux::desktop_handoff(&runtime, host, epoch).await?;
            #[cfg(windows)]
            windows::desktop_handoff(&runtime, host, epoch).await?;
            Ok(0)
        }),
        Some(Command::FinishUpdate { retry }) => execute(retirement::run(retry)),
        #[cfg(target_os = "macos")]
        Some(Command::ReplaceMacosBundle { caller }) => {
            corvid_update::macos::replace_from_installer(caller)
        }
        #[cfg(target_os = "macos")]
        Some(Command::PublishMacosDesktop { root, app }) => {
            corvid_update::macos::publish_managed(&root, &app)
        }
        Some(Command::Update(options)) => update::run(options),
        Some(Command::Companion(companion)) => execute(async move {
            companion::execute(companion, daemon_runtime).await?;
            Ok(0)
        }),
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        Some(Command::Linux(command)) => linux::execute(command, daemon_runtime, started),
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        None => execute(linux::start(daemon_runtime)),
        #[cfg(windows)]
        Some(Command::Windows(command)) => windows::dispatch(command, daemon_runtime, started),
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

#[cfg(all(
    feature = "compio-probe",
    any(target_os = "linux", target_os = "macos", windows)
))]
fn run_compio(run: impl Future<Output = Result<()>>) -> Result<u32> {
    compio::runtime::Runtime::new()?.block_on(run)?;
    Ok(0)
}

#[cfg(all(
    feature = "compio-probe",
    any(target_os = "linux", target_os = "macos", windows)
))]
fn compio_ping(runtime_dir: Option<PathBuf>) -> Result<u32> {
    let runtime = corvid_client::runtime_path(runtime_dir)?;
    let code = run_compio(corvid_client::compio_ping(&runtime))?;
    println!("pong");
    Ok(code)
}

#[cfg(feature = "compio-daemon")]
fn run_compio_daemon(
    runtime: &std::path::Path,
    state: &std::path::Path,
    autostarted: bool,
) -> Result<u32> {
    let executor = compio::runtime::Runtime::new()?;
    executor.block_on(async {
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let daemon = corvid_daemon::run_native(runtime, state, autostarted, async move {
            let _ = stopped.await;
        });
        let mut daemon = std::pin::pin!(daemon);
        let signal = async {
            #[cfg(unix)]
            tokio::select! {
                result = compio::signal::unix::signal(nix::libc::SIGTERM) => result,
                result = compio::signal::ctrl_c() => result,
            }
            #[cfg(windows)]
            compio::signal::ctrl_c().await
        };
        tokio::select! {
            biased;
            result = signal => {
                result?;
                drop(stop);
                daemon.await?;
            }
            result = &mut daemon => result?,
        }
        Ok(0)
    })
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
