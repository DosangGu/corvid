use std::env;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use corvid_client::{AttachOptions, UpgradeTarget, daemon_missing};
use corvid_protocol::unix::{RuntimeDir, SocketRole};
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault};
use nix::errno::Errno;
use nix::fcntl::Flock;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

use super::runtime_path;

const TERMINFO: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/terminfo/c/corvid"));
const STARTUP_POLL: Duration = Duration::from_millis(25);

pub(crate) async fn start(override_path: Option<PathBuf>) -> Result<u32> {
    corvid_client::check_tty()?;
    let path = runtime_path(override_path)?;
    let runtime = RuntimeDir::create(&path)?;
    ensure_daemon(&runtime).await?;

    let options = AttachOptions {
        launch_if_empty: true,
        ..AttachOptions::default()
    };
    let result = corvid_client::attach(&path, options).await;
    let fault = result
        .as_ref()
        .err()
        .and_then(|error| error.downcast_ref::<ProtocolFault>());
    if fault.is_some_and(|fault| fault.code == "NoTerminal") {
        // Older daemons only support a separate, explicit shell launch.
        let options = AttachOptions {
            launch: Some(Vec::new()),
            ..AttachOptions::default()
        };
        return corvid_client::attach(&path, options).await;
    }
    result
}

pub(crate) async fn ensure(path: &Path) -> Result<()> {
    ensure_daemon(&RuntimeDir::create_all(path)?).await
}

async fn ensure_daemon(runtime: &RuntimeDir) -> Result<()> {
    timeout(COMMAND_TIMEOUT, async {
        let _startup = lock_startup(runtime).await?;
        ensure_daemon_locked(runtime, None).await.map(|_| ())
    })
    .await
    .context("waiting for the Corvid daemon to start")?
}

pub(super) async fn upgrade(path: &Path) -> Result<()> {
    let runtime = RuntimeDir::create(path)?;
    let _startup = timeout(COMMAND_TIMEOUT, lock_startup(&runtime))
        .await
        .context("waiting for exclusive daemon startup")??;
    if !daemon_ready(path).await? {
        timeout(COMMAND_TIMEOUT, ensure_daemon_locked(&runtime, None))
            .await
            .context("waiting for the Corvid daemon to start")??;
        println!("Started the Corvid daemon from this build");
        return Ok(());
    }

    let target = corvid_client::prepare_upgrade(&runtime).await?;
    timeout(COMMAND_TIMEOUT, wait_for_shutdown(&runtime))
        .await
        .context("waiting for the previous Corvid daemon to shut down")??;
    timeout(COMMAND_TIMEOUT, start_replacement(&runtime, &target))
        .await
        .context("waiting for the updated Corvid daemon to start")??;
    println!("Upgraded the Corvid daemon");
    Ok(())
}

async fn lock_startup(runtime: &RuntimeDir) -> Result<Flock<File>> {
    loop {
        if let Some(lock) = try_lock(runtime.lock_startup())? {
            return Ok(lock);
        }
        sleep(STARTUP_POLL).await;
    }
}

async fn wait_for_shutdown(runtime: &RuntimeDir) -> Result<()> {
    loop {
        if try_lock(runtime.lock_instance())?.is_some() {
            return Ok(());
        }
        sleep(STARTUP_POLL).await;
    }
}

async fn start_replacement(runtime: &RuntimeDir, target: &UpgradeTarget) -> Result<()> {
    let pid = ensure_daemon_locked(runtime, Some(&target.state_directory))
        .await?
        .context("another daemon started during upgrade")?;
    let peer = runtime.connect(SocketRole::Control).await?.peer_cred()?;
    ensure!(
        peer.pid().and_then(|pid| u32::try_from(pid).ok()) == Some(pid),
        "another daemon took over the runtime during upgrade"
    );
    let host = corvid_client::host(runtime.path()).await?;
    ensure!(
        host.host_id == target.host_id && host.server_epoch != target.server_epoch,
        "replacement daemon did not reopen the previous host state"
    );
    Ok(())
}

async fn ensure_daemon_locked(
    runtime: &RuntimeDir,
    state_directory: Option<&Path>,
) -> Result<Option<u32>> {
    let mut child: Option<StartingDaemon> = None;
    loop {
        if let Some(child) = &mut child {
            child.check_running()?;
        }
        if daemon_ready(runtime.path()).await? {
            return Ok(child.and_then(|child| child.process.id()));
        }
        if child.is_none()
            && let Some(instance) = try_lock(runtime.lock_instance())?
        {
            drop(instance);
            child = Some(StartingDaemon::spawn(runtime, state_directory)?);
        }
        sleep(STARTUP_POLL).await;
    }
}

fn try_lock<T>(lock: Result<T>) -> Result<Option<T>> {
    match lock {
        Ok(guard) => Ok(Some(guard)),
        Err(error) if error.downcast_ref::<Errno>() == Some(&Errno::EWOULDBLOCK) => Ok(None),
        Err(error) => Err(error),
    }
}

async fn daemon_ready(runtime: &Path) -> Result<bool> {
    match corvid_client::ping(runtime).await {
        Ok(()) => Ok(true),
        Err(error) if daemon_missing(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

struct StartingDaemon {
    process: Child,
    diagnostics: File,
}

impl StartingDaemon {
    fn spawn(runtime: &RuntimeDir, state_directory: Option<&Path>) -> Result<Self> {
        let terminfo = runtime.path().join("terminfo");
        let entries = RuntimeDir::create_all(&terminfo.join("c"))?;
        entries.replace_private_file("corvid", TERMINFO)?;

        let diagnostics = tempfile::tempfile()?;
        let mut command = Command::new(env::current_exe()?);
        command
            .arg("daemon")
            .arg("--runtime-dir")
            .arg(runtime.path())
            .arg("--autostarted")
            .env("TERMINFO", &terminfo)
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(diagnostics.try_clone()?));
        if let Some(state_directory) = state_directory {
            command.arg("--state-dir").arg(state_directory);
        }
        // setsid detaches the child from the caller's TTY before exec.
        // The hook uses only async-signal-safe operations.
        unsafe {
            command.pre_exec(|| {
                if nix::libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let process = command.spawn().context("starting the background daemon")?;
        Ok(Self {
            process,
            diagnostics,
        })
    }

    fn check_running(&mut self) -> Result<()> {
        if let Some(status) = self.process.try_wait()? {
            let mut buffer = [0; 4096];
            let length = self.diagnostics.read_at(&mut buffer, 0)?;
            let diagnostic = String::from_utf8_lossy(&buffer[..length]);
            bail!("background daemon exited ({status}): {}", diagnostic.trim());
        }
        Ok(())
    }
}
