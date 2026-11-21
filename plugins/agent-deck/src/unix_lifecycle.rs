//! Reuse or retire the running Deck of a verified Unix controller.
use std::fs::File;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use async_process::Command;
use corvid_async::time::{sleep, timeout};
use corvid_companion::Config;
use corvid_companion::monitor::{Scope, discover};
use corvid_protocol::COMMAND_TIMEOUT;
use corvid_protocol::paths::agent_deck_socket;
use corvid_protocol::unix::{RuntimeDir, SocketRole, peer_pid};
use nix::errno::Errno;
use nix::fcntl::Flock;
use semver::Version;
use uuid::Uuid;

use crate::connection::{Client, LIFECYCLE_CAPABILITY};

#[cfg(target_os = "linux")]
#[path = "unix_lifecycle/linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "unix_lifecycle/macos.rs"]
mod platform;

const RETRY: Duration = Duration::from_millis(50);

/// Refresh the shared Deck once, then retire the other running Decks in its profile.
pub async fn refresh_profile(
    executable: &Path,
    root: &Path,
    requested: Option<Scope>,
) -> Result<()> {
    let mut scopes = discover(root)?;
    let address_override = requested
        .as_ref()
        .map(|scope| (scope.config.controller_id, scope.address.clone()));
    if let Some(requested) = requested {
        scopes.retain(|scope| scope.config.controller_id != requested.config.controller_id);
        scopes.insert(0, requested);
    }

    let mut failures = Vec::new();
    let mut running = Vec::new();
    for scope in scopes {
        let host = scope.config.host_id;
        let controller = scope.config.controller_id;
        let inspected = async { inspect(&directory(controller)?, host, controller).await }.await;
        match inspected {
            Ok(Some((_, peer))) => running.push((scope, peer.visible())),
            Ok(None) => {}
            Err(error) => failures.push(format!("{controller}: {error:#}")),
        }
    }

    if !running.is_empty() {
        let preferred = running
            .iter()
            .position(|(_, visible)| *visible)
            .unwrap_or(0);
        let peers: Vec<_> = running
            .iter()
            .map(|(scope, visible)| (scope.config.host_id, scope.config.controller_id, *visible))
            .collect();
        let mut owner = crate::scope::shared_scope(running.swap_remove(preferred).0).await?;
        let controller = owner.config.controller_id;
        if let Some((requested, address)) = address_override
            && requested == controller
        {
            owner.address = address;
        }
        let any_visible = peers.iter().any(|(_, _, visible)| *visible);
        let owner_was_hidden = peers
            .iter()
            .any(|(_, id, visible)| *id == controller && !visible);
        let mut client = ensure_current_or_start(
            executable,
            &owner.state,
            &owner.address,
            &owner.config,
            Some(!any_visible),
        )
        .await?
        .context("the shared Agent Deck did not start")?;
        if any_visible && owner_was_hidden {
            client.activate(false).await?;
        }
        for (host, duplicate, _) in peers {
            if duplicate != controller
                && let Err(error) = retire_duplicate(host, duplicate).await
            {
                failures.push(format!("{duplicate}: {error:#}"));
            }
        }
    }

    ensure!(
        failures.is_empty(),
        "Could not update running Agent Decks: {}",
        failures.join("; ")
    );
    Ok(())
}

async fn retire_duplicate(host: Uuid, controller: Uuid) -> Result<()> {
    let directory = directory(controller)?;
    let _update = lock_update(&directory).await?;
    let Some((mut client, peer)) = inspect(&directory, host, controller).await? else {
        return Ok(());
    };
    let own_version = Version::parse(env!("CARGO_PKG_VERSION"))?;
    match reported_version(&client)? {
        Some(version) if version.cmp_precedence(&own_version).is_gt() => return Ok(()),
        Some(_) => client.retire().await?,
        None if peer.retire_legacy(&own_version).await?.is_none() => return Ok(()),
        None => {}
    }
    wait_for_instance(&directory).await
}

pub async fn ensure_current(
    executable: &Path,
    state: &Path,
    address: &Path,
    config: &Config,
    start_if_missing: bool,
) -> Result<Option<Client>> {
    ensure_current_or_start(
        executable,
        state,
        address,
        config,
        start_if_missing.then_some(false),
    )
    .await
}

async fn ensure_current_or_start(
    executable: &Path,
    state: &Path,
    address: &Path,
    config: &Config,
    background_if_missing: Option<bool>,
) -> Result<Option<Client>> {
    let executable = executable
        .canonicalize()
        .context("locating the bundled Agent Deck")?;
    let host = config.host_id;
    let controller = config.controller_id;
    let directory = directory(controller)?;
    let _update = lock_update(&directory).await?;

    let background = match inspect(&directory, host, controller).await? {
        Some((mut client, peer)) => {
            let own_version = Version::parse(env!("CARGO_PKG_VERSION"))?;
            if let Some(version) = reported_version(&client)? {
                if version.cmp_precedence(&own_version).is_ge() {
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
        None => match background_if_missing {
            Some(background) => background,
            None => return Ok(None),
        },
    };

    wait_for_instance(&directory).await?;

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
            if let Some((client, _)) = inspect(&directory, host, controller).await? {
                return Ok(Some(client));
            }
            if let Some(status) = child.try_status()? {
                anyhow::bail!("Corvid Agent Deck exited during startup ({status})");
            }
            sleep(RETRY).await;
        }
    })
    .await
    .context("updated Agent Deck did not become ready")?
}

fn directory(controller: Uuid) -> Result<RuntimeDir> {
    RuntimeDir::create_all(&agent_deck_socket(controller)?.prepare()?)
}

async fn lock_update(directory: &RuntimeDir) -> Result<Flock<File>> {
    // The update and instance locks have different lifetimes. A replacement
    // can bind while its launcher still owns the update lock.
    timeout(COMMAND_TIMEOUT, async {
        loop {
            match directory.lock_startup() {
                Ok(lock) => break Ok(lock),
                Err(error) if lock_busy(&error) => sleep(RETRY).await,
                Err(error) => break Err(error),
            }
        }
    })
    .await
    .context("waiting for the Agent Deck update lock")?
}

async fn wait_for_instance(directory: &RuntimeDir) -> Result<()> {
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
    .context("the previous Agent Deck did not release its instance")?
}

fn reported_version(client: &Client) -> Result<Option<Version>> {
    if !client
        .session
        .capabilities
        .iter()
        .any(|cap| cap == LIFECYCLE_CAPABILITY)
    {
        return Ok(None);
    }
    let version = client
        .session
        .daemon_version
        .as_deref()
        .context("missing Agent Deck version")?;
    Ok(Some(Version::parse(version)?))
}

fn lock_busy(error: &anyhow::Error) -> bool {
    matches!(error.downcast_ref::<Errno>(), Some(&Errno::EWOULDBLOCK))
        || (cfg!(target_os = "macos") && error.downcast_ref::<Errno>() == Some(&Errno::ENOENT))
}

async fn inspect(
    directory: &RuntimeDir,
    host: Uuid,
    controller: Uuid,
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
    let client = Client::handshake(Box::new(stream), host, controller).await?;
    Ok(Some((client, peer)))
}
