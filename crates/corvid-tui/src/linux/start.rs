use std::env;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use async_process::Child;
use corvid_async::time::{sleep, timeout};
use corvid_client::{AttachOptions, UpgradeTarget, daemon_missing};
use corvid_protocol::unix::{RuntimeDir, SocketRole};
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault};
use nix::errno::Errno;
use nix::fcntl::Flock;
use std::os::unix::process::CommandExt;
use std::process::Command;
use uuid::Uuid;

const STARTUP_POLL: Duration = Duration::from_millis(25);

pub(crate) async fn start(override_path: Option<PathBuf>) -> Result<u32> {
    corvid_client::check_tty()?;
    let path = corvid_client::view_state::tui_runtime(override_path)?;
    let runtime = RuntimeDir::create(&path)?;
    ensure_daemon(&runtime).await?;

    let options = AttachOptions {
        remember_view: true,
        launch_if_empty: true,
        ..AttachOptions::default()
    };
    let result = crate::attach_monitored(&path, options).await;
    let fault = result
        .as_ref()
        .err()
        .and_then(|error| error.downcast_ref::<ProtocolFault>());
    if fault.is_some_and(|fault| fault.code == "NoTerminal")
        && corvid_client::view_state::tui_store()
            .and_then(|store| store.for_target(&corvid_client::view_state::tui_target(&path).ok()?))
            .is_none()
    {
        // Older daemons only support a separate, explicit shell launch.
        let options = AttachOptions {
            remember_view: true,
            launch: Some(Vec::new()),
            ..AttachOptions::default()
        };
        return crate::attach_monitored(&path, options).await;
    }
    result
}

pub(crate) async fn ensure(path: &Path) -> Result<()> {
    ensure_daemon(&RuntimeDir::create_all(path)?).await
}

async fn ensure_daemon(runtime: &RuntimeDir) -> Result<()> {
    timeout(COMMAND_TIMEOUT, async {
        let _startup = lock_startup(runtime).await?;
        if daemon_ready(runtime.path()).await? {
            return Ok(());
        }
        ensure_daemon_locked(runtime, None, &crate::local_daemon_executable()?)
            .await
            .map(|_| ())
    })
    .await
    .context("waiting for the Corvid daemon to start")?
}

pub(crate) async fn upgrade(path: &Path) -> Result<()> {
    let runtime = RuntimeDir::create_all(path)?;
    let _startup = upgrade_lock(&runtime).await?;
    let executable = crate::local_daemon_executable()?;
    let target = stop_for_upgrade(&runtime, None).await?;
    finish_upgrade(&runtime, target.as_ref(), &executable).await
}

pub(crate) async fn desktop_handoff(path: &Path, host: Uuid, epoch: Uuid) -> Result<()> {
    let executable = crate::local_daemon_executable()?;
    let runtime = RuntimeDir::open(path)?;
    let _startup = upgrade_lock(&runtime).await?;
    let previous_image = running_image(&runtime).await;
    let target = corvid_client::prepare_daemon_handoff(runtime.path(), host, epoch).await?;
    wait_for_upgrade_shutdown(&runtime).await?;
    if let Err(error) = finish_upgrade(&runtime, Some(&target), &executable).await {
        if let Some(previous_image) = previous_image
            && try_lock(runtime.lock_instance())?.is_some()
        {
            start_replacement(&runtime, &target, &previous_image)
                .await
                .context("restarting the previous daemon after the Desktop update failed")?;
        }
        return Err(error);
    }
    Ok(())
}

async fn running_image(runtime: &RuntimeDir) -> Option<PathBuf> {
    let stream = runtime.connect(SocketRole::Control).await.ok()?;
    let pid = corvid_protocol::unix::peer_pid(&stream).ok()?;
    let path = corvid_protocol::unix::process_image_path(pid).ok()?;
    path.is_file().then_some(path)
}

pub(super) async fn upgrade_managed(
    path: &Path,
    previous_version: &str,
    expected_host: Option<Uuid>,
) -> Result<()> {
    let staged = env::current_exe()?;
    let home = env::var_os("HOME").context("HOME is unavailable")?;
    let root = PathBuf::from(home).join(".corvid");
    ensure!(
        staged.parent() == Some(root.as_path())
            && staged
                .file_name()
                .is_some_and(|name| name.as_encoded_bytes().starts_with(b".corvid.")),
        "the managed upgrade must run from a staged Corvid binary"
    );
    let installed = root.join("corvid");
    let runtime = RuntimeDir::create_all(path)?;
    let _startup = upgrade_lock(&runtime).await?;
    crate::verify_installed_version(&installed, previous_version).await?;

    let backup = tempfile::tempdir_in(&root)?;
    let previous = backup.path().join("corvid");
    fs::hard_link(&installed, &previous).context("saving the previous Corvid binary")?;
    let target = stop_for_upgrade(&runtime, expected_host).await?;
    if target.is_none()
        && let Some(host) = expected_host
    {
        ensure!(
            corvid_daemon::stored_host_id(&super::state_path(None)?)? == Some(host),
            ProtocolFault::new(
                "HostMismatch",
                "SSH alias points to a different Corvid host"
            )
        );
    }
    {
        let _instance = try_lock(runtime.lock_instance())?.context(ProtocolFault::new(
            "DaemonBusy",
            "the Corvid daemon is still active",
        ))?;
        fs::rename(&staged, &installed).context("publishing the updated Corvid binary")?;
    }
    let result = finish_upgrade(&runtime, target.as_ref(), &installed).await;
    if result.is_err()
        && let Some(_instance) = try_lock(runtime.lock_instance())?
    {
        fs::rename(previous, installed).context("restoring the previous Corvid binary")?;
        drop(_instance);
        if target.is_some() {
            finish_upgrade(&runtime, target.as_ref(), &root.join("corvid"))
                .await
                .context("restarting the previous Corvid daemon after the update failed")?;
        }
    }
    result
}

async fn upgrade_lock(runtime: &RuntimeDir) -> Result<Flock<File>> {
    timeout(COMMAND_TIMEOUT, lock_startup(runtime))
        .await
        .context("waiting for exclusive daemon startup")?
}

async fn stop_for_upgrade(
    runtime: &RuntimeDir,
    expected_host: Option<Uuid>,
) -> Result<Option<UpgradeTarget>> {
    match daemon_ready(runtime.path()).await {
        Ok(false) => return Ok(None),
        Ok(true) => {}
        Err(error)
            if error
                .downcast_ref::<ProtocolFault>()
                .is_some_and(|fault| fault.code == "ProtocolMismatch") => {}
        Err(error) => return Err(error),
    }
    let target = corvid_client::prepare_upgrade_for(runtime, expected_host).await?;
    wait_for_upgrade_shutdown(runtime).await?;
    Ok(Some(target))
}

async fn wait_for_upgrade_shutdown(runtime: &RuntimeDir) -> Result<()> {
    timeout(COMMAND_TIMEOUT, wait_for_shutdown(runtime))
        .await
        .context("waiting for the previous Corvid daemon to shut down")?
}

async fn finish_upgrade(
    runtime: &RuntimeDir,
    target: Option<&UpgradeTarget>,
    executable: &Path,
) -> Result<()> {
    timeout(COMMAND_TIMEOUT, async {
        if let Some(target) = target {
            start_replacement(runtime, target, executable).await?;
            println!("Upgraded the Corvid daemon");
        } else {
            ensure_daemon_locked(runtime, None, executable).await?;
            println!("Started the Corvid daemon from this build");
        }
        Ok(())
    })
    .await
    .context("waiting for the updated Corvid daemon to start")?
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

async fn start_replacement(
    runtime: &RuntimeDir,
    target: &UpgradeTarget,
    executable: &Path,
) -> Result<()> {
    let pid = ensure_daemon_locked(runtime, Some(&target.state_directory), executable)
        .await?
        .context("another daemon started during upgrade")?;
    let stream = runtime.connect(SocketRole::Control).await?;
    ensure!(
        corvid_protocol::unix::peer_pid(&stream)? == pid,
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
    executable: &Path,
) -> Result<Option<u32>> {
    let mut child: Option<StartingDaemon> = None;
    loop {
        if let Some(child) = &mut child {
            child.check_running()?;
        }
        if daemon_ready(runtime.path()).await? {
            return Ok(child.map(StartingDaemon::detach));
        }
        if child.is_none()
            && let Some(instance) = try_lock(runtime.lock_instance())?
        {
            drop(instance);
            child = Some(StartingDaemon::spawn(runtime, state_directory, executable)?);
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
    ready: bool,
}

impl StartingDaemon {
    fn spawn(
        runtime: &RuntimeDir,
        state_directory: Option<&Path>,
        executable: &Path,
    ) -> Result<Self> {
        // Keep the environment expected by older daemon images during rollback.
        let terminfo = corvid_daemon::terminfo::install(runtime)?;

        let diagnostics = tempfile::tempfile()?;
        let mut command = Command::new(executable);
        command
            .arg("daemon")
            .arg("--runtime-dir")
            .arg(runtime.path())
            .arg("--autostarted")
            .env("TERMINFO", &terminfo)
            .current_dir("/");
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
        let process = async_process::Command::from(command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(diagnostics.try_clone()?))
            .spawn()
            .context("starting the background daemon")?;
        Ok(Self {
            process,
            diagnostics,
            ready: false,
        })
    }

    fn detach(mut self) -> u32 {
        self.ready = true;
        self.process.id()
    }

    fn check_running(&mut self) -> Result<()> {
        if let Some(status) = self.process.try_status()? {
            let mut buffer = [0; 4096];
            let length = self.diagnostics.read_at(&mut buffer, 0)?;
            let diagnostic = String::from_utf8_lossy(&buffer[..length]);
            bail!("background daemon exited ({status}): {}", diagnostic.trim());
        }
        Ok(())
    }
}

impl Drop for StartingDaemon {
    fn drop(&mut self) {
        if !self.ready {
            let _ = self.process.kill();
        }
    }
}
