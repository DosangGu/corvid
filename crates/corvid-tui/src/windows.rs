use std::ffi::OsString;
use std::io::{self, ErrorKind};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Subcommand, ValueEnum};
use corvid_client::{AttachOptions, attach, check_tty, host, ping};
use corvid_protocol::COMMAND_TIMEOUT;
use corvid_protocol::windows::StateDirectory;
use tokio::time::sleep;
use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const STARTUP_POLL: Duration = Duration::from_millis(50);

#[derive(Subcommand)]
pub(super) enum Command {
    /// Run a local Corvid daemon in the foreground.
    Daemon {
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Start or reuse the local background daemon.
    EnsureDaemon {
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Attach to a terminal workspace.
    Attach {
        #[arg(long)]
        new: bool,
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
        #[arg(value_enum)]
        request: ControlRequest,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub(super) enum ControlRequest {
    Host,
    Ping,
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

pub(super) async fn execute(command: Command, runtime: Option<PathBuf>) -> Result<u32> {
    let runtime = runtime_path(runtime)?;
    match command {
        Command::Daemon { state_dir } => {
            let state = state_path(state_dir)?;
            let mut interrupt = tokio::signal::windows::ctrl_c()?;
            corvid_daemon::run(&runtime, &state, async {
                interrupt.recv().await;
            })
            .await?;
        }
        Command::EnsureDaemon { state_dir } => {
            ensure_daemon(&runtime, &state_path(state_dir)?).await?;
        }
        Command::Attach {
            new,
            workspace,
            tab,
            pane,
            new_workspace,
            new_tab,
            argv,
        } => {
            let launch = (new || !argv.is_empty()).then_some(argv);
            let options = AttachOptions {
                launch_if_empty: launch.is_none(),
                launch,
                workspace,
                tab,
                pane,
                new_workspace,
                new_tab,
                ..AttachOptions::default()
            };
            return start_attach(runtime, options).await;
        }
        Command::Control { request } => match request {
            ControlRequest::Host => {
                let host = host(&runtime).await?;
                println!(
                    "host_id={} server_epoch={}",
                    host.host_id, host.server_epoch
                );
            }
            ControlRequest::Ping => {
                ping(&runtime).await?;
                println!("pong");
            }
        },
    }
    Ok(0)
}

fn runtime_path(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) =
        override_path.or_else(|| std::env::var_os("CORVID_RUNTIME_DIR").map(PathBuf::from))
    {
        return Ok(path);
    }
    Ok(local_app_data()?.join("Corvid/runtime"))
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
        ensure_daemon(&runtime, &state_path(None)?).await?;
    }
    attach(&runtime, options).await
}

async fn daemon_running(runtime: &Path) -> Result<bool> {
    match ping(runtime).await {
        Ok(()) => Ok(true),
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == ErrorKind::NotFound) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

async fn ensure_daemon(runtime: &Path, state: &Path) -> Result<()> {
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
    let mut child = process::Command::new(std::env::current_exe()?)
        .arg("--runtime-dir")
        .arg(runtime)
        .args(["daemon", "--state-dir"])
        .arg(state)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .context("starting the Windows Corvid daemon")?;
    let result = wait_for_daemon(runtime, &mut child).await;
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

async fn wait_for_daemon(runtime: &Path, child: &mut process::Child) -> Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
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
