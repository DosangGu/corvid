use std::path::Path;

use anyhow::{Context, Result, ensure};
use corvid_client::{ControlEndpoint, InteractiveConnection, daemon_missing, runtime_path};
use corvid_protocol::{ProtocolFault, RESTORE_AGENTS_CAPABILITY, Session};
use semver::Version;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::daemon_prompt::{Operation, Prompts, needs_upgrade};

static LOCAL_CONNECTION: Mutex<()> = Mutex::const_new(());

pub async fn connect(
    capabilities: Vec<String>,
    explicit: bool,
    expected_host: Option<&str>,
    prompts: &Prompts,
    progress: impl Fn(&str),
) -> Result<(ControlEndpoint, InteractiveConnection)> {
    let runtime = runtime_path(None)?;
    let endpoint = ControlEndpoint::Local(runtime.clone());
    // Serialize automatic handoff and the first handshake among restored windows.
    let connection_guard = LOCAL_CONNECTION.lock().await;
    let connection = match endpoint.interactive(capabilities.clone()).await {
        Err(error) if explicit && daemon_missing(&error) => {
            progress("Starting the local Corvid daemon…");
            match corvid_tui::ensure_local_daemon(&runtime).await {
                Ok(()) => endpoint.interactive(capabilities.clone()).await,
                Err(error) => Err(error.context("starting the local Corvid daemon")),
            }
        }
        connection => connection,
    };
    let link = match connection {
        Err(error) if explicit && needs_upgrade(&error) => {
            drop(connection_guard);
            upgrade(&runtime, prompts, &progress).await?;
            let link = endpoint.interactive(capabilities).await?;
            verify_host(&link, expected_host)?;
            return Ok((endpoint, link));
        }
        connection => connection?,
    };
    verify_host(&link, expected_host)?;
    if !needs_handoff(link.session()) {
        return Ok((endpoint, link));
    }

    let host = link.session().host_id.clone();
    let epoch = link.session().server_epoch.clone();
    drop(link);
    progress("Upgrading the local Corvid daemon…");
    let handoff = handoff(&runtime, &host, &epoch).await;
    progress("Connecting to Corvid…");
    let link = match endpoint.interactive(capabilities).await {
        Ok(link) => link,
        Err(error) => {
            handoff?;
            return Err(error);
        }
    };
    verify_host(&link, Some(&host))?;
    Ok((endpoint, link))
}

fn verify_host(link: &InteractiveConnection, expected_host: Option<&str>) -> Result<()> {
    ensure!(
        expected_host.is_none_or(|host| host == link.session().host_id),
        ProtocolFault::new(
            "HostMismatch",
            "the connection points to a different Corvid host"
        )
    );
    Ok(())
}

fn needs_handoff(session: &Session) -> bool {
    let current = Version::parse(env!("CARGO_PKG_VERSION")).expect("valid Cargo package version");
    let running = match &session.daemon_version {
        Some(version) => Version::parse(version).ok(),
        // v0.1.5 introduced agent restoration. It is the last release without a version in Hello.
        None if session
            .capabilities
            .iter()
            .any(|cap| cap == RESTORE_AGENTS_CAPABILITY) =>
        {
            Some(Version::new(0, 1, 5))
        }
        None => Some(Version::new(0, 1, 4)),
    };
    running.is_some_and(|running| running.cmp_precedence(&current).is_lt())
}

async fn handoff(runtime: &Path, host: &str, epoch: &str) -> Result<()> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args([
            "desktop-handoff",
            "--host",
            host,
            "--epoch",
            epoch,
            "--runtime-dir",
        ])
        .arg(runtime);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW

    // Let the helper finish even if the window closes after the old daemon accepts.
    let output = command
        .output()
        .await
        .context("starting the daemon handoff helper")?;
    ensure!(
        output.status.success(),
        "Could not upgrade the local Corvid daemon. {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

async fn upgrade(
    runtime: &std::path::Path,
    prompts: &Prompts,
    progress: impl Fn(&str),
) -> Result<()> {
    prompts
        .confirm(
            Operation::Upgrade,
            "Local",
            "Replace the local Corvid daemon.\n\nHosted shells keep running.\nClients reconnect after the upgrade.",
        )
        .await?;
    progress("Upgrading the local Corvid daemon…");
    corvid_tui::upgrade_local_daemon(runtime).await
}
