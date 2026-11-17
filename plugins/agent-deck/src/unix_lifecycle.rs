//! Reuse or retire the running Deck of a verified Unix controller.
use std::io::ErrorKind;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use corvid_companion::Config;
use corvid_protocol::COMMAND_TIMEOUT;
use corvid_protocol::paths::agent_deck_socket;
use corvid_protocol::unix::{RuntimeDir, SocketRole, peer_pid};
use nix::errno::Errno;
use semver::Version;
use tokio::process::Command;
use tokio::time::{sleep, timeout};

use crate::connection::{Client, LIFECYCLE_CAPABILITY};

#[cfg(target_os = "linux")]
#[path = "unix_lifecycle/linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "unix_lifecycle/macos.rs"]
mod platform;

const RETRY: Duration = Duration::from_millis(50);

pub async fn ensure_current(
    executable: &Path,
    state: &Path,
    address: &Path,
    config: &Config,
    start_if_missing: bool,
) -> Result<Option<Client>> {
    let executable = executable
        .canonicalize()
        .context("locating the bundled Agent Deck")?;
    let directory = RuntimeDir::create_all(&agent_deck_socket(config.controller_id)?.prepare()?)?;
    // The update and instance locks have different lifetimes. A replacement
    // can bind while its launcher still owns the update lock.
    let _update = timeout(COMMAND_TIMEOUT, async {
        loop {
            match directory.lock_startup() {
                Ok(lock) => break Ok(lock),
                Err(error) if lock_busy(&error) => sleep(RETRY).await,
                Err(error) => break Err(error),
            }
        }
    })
    .await
    .context("waiting for the Agent Deck update lock")??;

    let background = match inspect(&directory, config).await? {
        Some((mut client, peer)) => {
            let own_version = Version::parse(env!("CARGO_PKG_VERSION"))?;
            if client
                .session
                .capabilities
                .iter()
                .any(|cap| cap == LIFECYCLE_CAPABILITY)
            {
                let version = client
                    .session
                    .daemon_version
                    .as_deref()
                    .context("missing Agent Deck version")?;
                if Version::parse(version)?
                    .cmp_precedence(&own_version)
                    .is_ge()
                {
                    return Ok(Some(client));
                }
                let visible = peer.visible();
                client.retire().await?;
                !visible
            } else {
                let Some(visible) = peer.retire_legacy(&own_version).await? else {
                    return Ok(Some(client));
                };
                !visible
            }
        }
        None if !start_if_missing => return Ok(None),
        None => false,
    };

    timeout(COMMAND_TIMEOUT, async {
        loop {
            match directory.lock_instance() {
                Ok(_) => break Ok(()),
                Err(error) if lock_busy(&error) => sleep(RETRY).await,
                Err(error) => break Err(error),
            }
        }
    })
    .await
    .context("the previous Agent Deck did not release its instance")??;

    let mut command = Command::new(executable);
    command
        .arg("--state-dir")
        .arg(state)
        .arg("--socket-dir")
        .arg(address)
        .arg("--managed-start")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if background {
        command.arg("--background");
    }
    let mut child = command
        .spawn()
        .context("starting updated Corvid Agent Deck")?;
    timeout(COMMAND_TIMEOUT, async {
        loop {
            if let Some((client, _)) = inspect(&directory, config).await? {
                return Ok(Some(client));
            }
            if let Some(status) = child.try_wait()? {
                anyhow::bail!("Corvid Agent Deck exited during startup ({status})");
            }
            sleep(RETRY).await;
        }
    })
    .await
    .context("updated Agent Deck did not become ready")?
}

fn lock_busy(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Errno>() == Some(&Errno::EWOULDBLOCK)
}

async fn inspect(
    directory: &RuntimeDir,
    config: &Config,
) -> Result<Option<(Client, platform::Peer)>> {
    let stream = match directory.connect(SocketRole::Presentation).await {
        Ok(stream) => stream,
        Err(error)
            if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::ConnectionRefused
                )
            }) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    // Capture the OS process reference before the authenticated peer answers.
    // A later process reusing its PID must never receive an exit request.
    let peer = platform::Peer::new(peer_pid(&stream)?);
    let client = Client::handshake(Box::new(stream), config.host_id, config.controller_id).await?;
    Ok(Some((client, peer)))
}
