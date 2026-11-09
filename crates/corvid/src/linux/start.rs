use std::env;
use std::fs::File;
use std::io::{self, ErrorKind};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use corvid_client::AttachOptions;
use corvid_protocol::unix::RuntimeDir;
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault};
use nix::errno::Errno;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

use super::runtime_path;

const TERMINFO: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/terminfo/c/corvid"));
const STARTUP_POLL: Duration = Duration::from_millis(25);

pub(crate) async fn start(override_path: Option<PathBuf>) -> Result<u32> {
    corvid_client::check_tty()?;
    let path = runtime_path(override_path)?;
    let runtime = RuntimeDir::create(&path)?;
    timeout(COMMAND_TIMEOUT, ensure_daemon(&runtime))
        .await
        .context("waiting for the Corvid daemon to start")??;

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

async fn ensure_daemon(runtime: &RuntimeDir) -> Result<()> {
    let _startup = loop {
        if daemon_ready(runtime.path()).await? {
            return Ok(());
        }
        if let Some(lock) = try_lock(runtime.lock_startup())? {
            break lock;
        }
        sleep(STARTUP_POLL).await;
    };

    let mut child: Option<StartingDaemon> = None;
    loop {
        if daemon_ready(runtime.path()).await? {
            return Ok(());
        }
        if let Some(instance) = try_lock(runtime.lock_instance())? {
            drop(instance);
            if let Some(child) = &mut child {
                child.check_running()?;
            } else {
                child = Some(StartingDaemon::spawn(runtime)?);
            }
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
        Err(error)
            if error.downcast_ref::<Errno>() == Some(&Errno::ENOENT)
                || error.downcast_ref::<io::Error>().is_some_and(|error| {
                    matches!(
                        error.kind(),
                        ErrorKind::NotFound | ErrorKind::ConnectionRefused
                    )
                }) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

struct StartingDaemon {
    process: Child,
    diagnostics: File,
}

impl StartingDaemon {
    fn spawn(runtime: &RuntimeDir) -> Result<Self> {
        let terminfo = runtime.path().join("terminfo");
        let entries = RuntimeDir::create_all(&terminfo.join("c"))?;
        entries.replace_private_file("corvid", TERMINFO)?;

        let diagnostics = tempfile::tempfile()?;
        let mut command = Command::new(env::current_exe()?);
        command
            .arg("--runtime-dir")
            .arg(runtime.path())
            .arg("daemon")
            .env("TERMINFO", &terminfo)
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(diagnostics.try_clone()?));
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
