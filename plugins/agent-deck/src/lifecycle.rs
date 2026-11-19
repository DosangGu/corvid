//! Windows Deck replacement, including releases predating lifecycle IPC.
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_companion::Config;
use corvid_protocol::COMMAND_TIMEOUT;
use corvid_protocol::windows::StateDirectory;
use semver::Version;
use tokio::process::Command;
use tokio::time::{sleep, timeout};

use crate::connection::Client;

mod process;
use process::Process;

/// Older Desktop packages use the same GPUI message loop as Deck.
pub async fn close_legacy_desktop(pid: u32, image: &Path) -> Result<()> {
    let process = Process::open(pid)?;
    ensure!(
        image
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("corvid-desktop.exe"))
            && std::fs::canonicalize(process.image_path()?)? == std::fs::canonicalize(image)?,
        "the running Desktop image changed"
    );
    process.quit().await
}

/// Reuse a current/newer Deck or replace an older one from this controller's
/// private endpoint. A startup sweep never starts a previously stopped Deck.
pub async fn ensure_current(
    executable: &Path,
    state: &Path,
    address: &str,
    config: &Config,
    start_if_missing: bool,
) -> Result<Option<Client>> {
    // Serialize startup sweeps and Agents clicks from multiple Desktop windows.
    // This is separate from the Deck's own lifetime lock.
    let lock_path = state.join("agent-deck-update");
    let (_storage, _lock) = tokio::task::spawn_blocking(move || {
        let storage = StateDirectory::create_all(&lock_path)?;
        let lock = storage.lock_storage_with_timeout(COMMAND_TIMEOUT)?;
        Ok::<_, anyhow::Error>((storage, lock))
    })
    .await??;

    let mut background = false;
    match inspect(config).await? {
        Some((client, process)) => {
            let image = process.image_path()?;
            ensure!(
                image
                    .file_name()
                    .is_some_and(|name| name.eq_ignore_ascii_case("corvid-agent-deck.exe")),
                "Agent Deck endpoint belongs to an unexpected executable"
            );
            let expected = std::fs::canonicalize(executable)?;
            if std::fs::canonicalize(&image)? == expected
                || version(&image)
                    .await?
                    .cmp_precedence(&Version::parse(env!("CARGO_PKG_VERSION"))?)
                    != std::cmp::Ordering::Less
            {
                return Ok(Some(client));
            }
            // Querying the private endpoint and retaining the process handle
            // fences PID reuse. Never terminate a daemon or shared companion.
            background = !process.visible().await?;
            drop(client);
            process.quit().await?;
        }
        None if !start_if_missing => return Ok(None),
        None => {}
    }

    let mut command = Command::new(executable);
    command
        .arg("--state-dir")
        .arg(state)
        .arg("--pipe-instance")
        .arg(address)
        .arg("--managed-start")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x0800_0000);
    if background {
        command.arg("--background");
    }
    let mut child = command
        .spawn()
        .context("starting updated Corvid Agent Deck")?;
    timeout(COMMAND_TIMEOUT, async {
        loop {
            if let Some((client, _)) = inspect(config).await? {
                return Ok(Some(client));
            }
            if let Some(status) = child.try_wait()? {
                anyhow::bail!("Corvid Agent Deck exited during startup ({status})");
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("updated Agent Deck did not become ready")?
}

async fn inspect(config: &Config) -> Result<Option<(Client, Process)>> {
    let result = timeout(
        COMMAND_TIMEOUT,
        corvid_protocol::windows::open_scoped(
            "corvid-agent-deck-v1",
            &config.controller_id.simple().to_string(),
        ),
    )
    .await
    .context("Agent Deck endpoint timed out")?;
    let pipe = match result {
        Ok(pipe) => pipe,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let process = Process::from_pipe(&pipe)?;
    let client = Client::handshake(Box::new(pipe), config.host_id, config.controller_id).await?;
    Ok(Some((client, process)))
}

async fn version(executable: &Path) -> Result<Version> {
    let output = timeout(
        COMMAND_TIMEOUT,
        Command::new(executable)
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("checking running Agent Deck version timed out")??;
    ensure!(
        output.status.success(),
        "cannot check running Agent Deck version"
    );
    let text = std::str::from_utf8(&output.stdout)?.trim();
    let version = text
        .strip_prefix("corvid-agent-deck ")
        .context("unexpected Agent Deck version response")?;
    Ok(Version::parse(version)?)
}
