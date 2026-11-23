mod process;

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use corvid_async::time::{sleep, sleep_until, timeout_at};
use corvid_protocol::windows::StateDirectory;
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault};
use uuid::Uuid;
use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

use self::process::DaemonProcess;
use super::state_path;
use crate as corvid_client;
use crate::{daemon_missing, host, ping};

pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const STARTUP_POLL: Duration = Duration::from_millis(50);

fn local_app_data() -> Result<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .context("LOCALAPPDATA is unavailable")
}

pub async fn upgrade(
    runtime: &Path,
    desktop: bool,
    resolve: impl FnOnce() -> Result<PathBuf>,
) -> Result<()> {
    let directory = StateDirectory::create_all(runtime)?;
    let _startup = lock_startup(&directory, Instant::now() + COMMAND_TIMEOUT).await?;
    let executable = resolve()?;
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
        executable,
        Instant::now() + STARTUP_TIMEOUT,
    )
    .await?;
    verify_replacement(runtime, target.as_ref()).await
}

pub async fn daemon_image(runtime: &Path) -> Result<PathBuf> {
    Ok(DaemonProcess::open(runtime).await?.image_path()?)
}

pub async fn desktop_handoff(
    runtime: &Path,
    host_id: Uuid,
    epoch: Uuid,
    resolve: impl FnOnce() -> Result<PathBuf>,
) -> Result<()> {
    let directory = StateDirectory::open(runtime)?;
    let _startup = lock_startup(&directory, Instant::now() + COMMAND_TIMEOUT).await?;
    let executable = resolve()?;
    let packaged =
        corvid_update::Installation::at(&executable).is_ok_and(|install| install.root.is_some());
    let previous = DaemonProcess::open(runtime).await?;
    let previous_image = previous.image_path()?;
    // Stage in the other slot so the running executable stays available for rollback.
    let alternate = "desktop-daemon-replacement.exe";
    let name = if previous_image.file_name() == Some(std::ffi::OsStr::new(alternate)) {
        "desktop-daemon.exe"
    } else {
        alternate
    };
    let next_image = if packaged {
        executable
    } else {
        directory.replace_private_file(name, &std::fs::read(executable)?)?;
        runtime.join(name)
    };
    let target = corvid_client::prepare_daemon_handoff(runtime, host_id, epoch).await?;
    wait_for_upgrade_shutdown(&directory, &previous).await?;
    let state = &target.state_directory;
    if let Err(error) = start_image(&next_image, runtime, state, false).await {
        start_image(&previous_image, runtime, state, false)
            .await
            .context("restarting the previous daemon after the Desktop update failed")?;
        return Err(error);
    }
    verify_replacement(runtime, Some(&target)).await
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
    wait_for_upgrade_shutdown(directory, &previous).await?;
    Ok(Some(target))
}

async fn wait_for_upgrade_shutdown(
    directory: &StateDirectory,
    previous: &DaemonProcess,
) -> Result<()> {
    corvid_async::time::timeout(COMMAND_TIMEOUT, async {
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
    .context("waiting for the previous Windows daemon to stop")?
}

fn sharing_violation(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32))
}

async fn lock_startup(directory: &StateDirectory, deadline: Instant) -> Result<std::fs::File> {
    timeout_at(deadline, async {
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

pub async fn upgrade_managed(
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
    let _startup = lock_startup(&directory, Instant::now() + COMMAND_TIMEOUT).await?;
    super::verify_installed_version(&root.join("corvid.exe"), previous_version).await?;
    let image = std::fs::read(&staged).context("reading the staged Corvid binary")?;
    let previous = installation
        .read_private_file("corvid.exe", 64 * 1024 * 1024)?
        .context("the managed Corvid binary is missing")?;
    let target = stop_for_upgrade(runtime, &directory, expected_host).await?;
    if target.is_none()
        && let Some(host) = expected_host
    {
        ensure!(
            super::identity::stored_host_id(&state_path(None)?)? == Some(host),
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

pub async fn ensure(
    runtime: &Path,
    state: &Path,
    ssh: bool,
    desktop_image: Option<&str>,
    resolve: impl FnOnce() -> Result<PathBuf>,
) -> Result<()> {
    let directory = StateDirectory::create_all(runtime)?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let _startup = lock_startup(&directory, deadline).await?;
    // The daemon owns its instance lock before publishing the control pipe.
    // A busy or unavailable pipe while the lock is held means readiness is
    // still unknown. Keep one deadline for both waiting and starting.
    while Instant::now() < deadline {
        match directory.lock_instance() {
            Ok(instance) => {
                drop(instance);
                return start_daemon(
                    runtime,
                    &directory,
                    state,
                    ssh,
                    desktop_image,
                    resolve()?,
                    deadline,
                )
                .await;
            }
            Err(error) if sharing_violation(&error) => {}
            Err(error) => return Err(error),
        }
        if probe(runtime, deadline).await? {
            return Ok(());
        }
        sleep_until(deadline.min(Instant::now() + STARTUP_POLL)).await;
    }
    bail!(
        "Windows daemon instance held its lock without answering the control pipe before the startup deadline"
    )
}

async fn start_daemon(
    runtime: &Path,
    directory: &StateDirectory,
    state: &Path,
    ssh: bool,
    desktop_image: Option<&str>,
    mut executable: PathBuf,
    deadline: Instant,
) -> Result<()> {
    if let Some(name) = desktop_image
        && !corvid_update::Installation::at(&executable).is_ok_and(|install| install.root.is_some())
    {
        // Keep the detached daemon from locking the GUI binary across rebuilds.
        let image = std::fs::read(&executable).context("reading the Desktop daemon image")?;
        directory.replace_private_file(name, &image)?;
        executable = runtime.join(name);
    }
    start_image_before(&executable, runtime, state, ssh, deadline).await
}

async fn start_image(executable: &Path, runtime: &Path, state: &Path, ssh: bool) -> Result<()> {
    start_image_before(
        executable,
        runtime,
        state,
        ssh,
        Instant::now() + STARTUP_TIMEOUT,
    )
    .await
}

async fn start_image_before(
    executable: &Path,
    runtime: &Path,
    state: &Path,
    ssh: bool,
    deadline: Instant,
) -> Result<()> {
    ensure!(
        Instant::now() < deadline,
        "Windows daemon startup deadline elapsed before launch"
    );
    let child = DaemonProcess::spawn(executable, runtime, state, ssh)
        .context("starting the Windows Corvid daemon")?;
    let result = wait_for_daemon(runtime, &child, deadline).await;
    if result.is_err() {
        child.terminate();
    }
    result
}

async fn wait_for_daemon(runtime: &Path, child: &DaemonProcess, deadline: Instant) -> Result<()> {
    loop {
        if let Some(status) = child.exit_code()? {
            bail!(
                "Corvid daemon exited during startup ({status}); run corvid daemon in the foreground for details"
            );
        }
        if probe(runtime, deadline).await? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "the launched Windows daemon did not answer the control pipe before the startup deadline"
            );
        }
        sleep_until(deadline.min(Instant::now() + STARTUP_POLL)).await;
    }
}

pub async fn probe(runtime: &Path, deadline: Instant) -> Result<bool> {
    let limit = deadline.min(Instant::now() + STARTUP_POLL * 4);
    match timeout_at(limit, daemon_running(runtime)).await {
        Ok(Err(error)) if error.is::<compio::time::Elapsed>() => Ok(false),
        Ok(result) => result,
        Err(_) => Ok(false),
    }
}
