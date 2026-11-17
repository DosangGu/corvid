use std::fs::File;
use std::io::Read;

use anyhow::{Context, Result, ensure};
use corvid_client::{DaemonInstallation, HostPlatform, InteractiveConnection, RemoteHost};
use corvid_protocol::ProtocolFault;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::daemon_prompt::{Operation, Prompts, has_fault, needs_upgrade};

const MAX_BINARY: u64 = 32 * 1024 * 1024;
const MAX_MANIFEST: u64 = 4096;

#[cfg(test)]
mod tests;

pub async fn connect(
    host: RemoteHost,
    capabilities: Vec<String>,
    explicit: bool,
    expected_host: Option<&str>,
    prompts: &Prompts,
    progress: impl Fn(&str),
) -> Result<(RemoteHost, InteractiveConnection)> {
    let (host, link) = connect_daemon(
        host,
        capabilities.clone(),
        explicit,
        expected_host,
        prompts,
        &progress,
    )
    .await?;
    verify_host(link.session().host_id.as_str(), expected_host)?;
    let Some(platform) = managed_platform(&host).filter(|_| explicit) else {
        return Ok((host, link));
    };

    // Wire compatibility does not imply that the installed provider collectors
    // include this Desktop release's fixes. Check even after a successful Hello.
    progress("Checking the remote Corvid version…");
    let host_id = link.session().host_id.clone();
    let upgrade = match available_upgrade(&host, platform, &host_id).await {
        Ok(Some(upgrade)) => upgrade,
        Ok(None) => return Ok((host, link)),
        Err(error) => {
            prompts
                .update_unavailable(host.name(), &format!("{error:#}"))
                .await;
            return Ok((host, link));
        }
    };
    if !prompts
        .offer_upgrade(host.name(), &upgrade_detail(&upgrade))
        .await
    {
        return Ok((host, link));
    }

    drop(link);
    apply_upgrade(&host, platform, &upgrade, &progress).await?;
    let link = host.interactive(capabilities).await?;
    verify_host(link.session().host_id.as_str(), Some(&host_id))?;
    Ok((host, link))
}

async fn connect_daemon(
    host: RemoteHost,
    capabilities: Vec<String>,
    explicit: bool,
    expected_host: Option<&str>,
    prompts: &Prompts,
    progress: impl Fn(&str),
) -> Result<(RemoteHost, InteractiveConnection)> {
    progress("Checking the execution host…");
    let platform = host.probe_platform().await?;
    let mut host = host.with_daemon(platform, DaemonInstallation::Path);
    let initial = match host.interactive(capabilities.clone()).await {
        Ok(link) => return Ok((host, link)),
        Err(error) => error,
    };
    if !needs_upgrade(&initial)
        && !has_fault(&initial, "RelayUnavailable")
        && !has_fault(&initial, "DaemonUnavailable")
    {
        return Err(initial);
    }

    progress("Checking the remote Corvid installation…");
    let (daemon, connection_error) = match host.probe_daemon(platform).await? {
        Some(DaemonInstallation::Managed) => {
            host = host.with_daemon(platform, DaemonInstallation::Managed);
            match host.interactive(capabilities.clone()).await {
                Ok(link) => return Ok((host, link)),
                Err(error) => (DaemonInstallation::Managed, error),
            }
        }
        Some(DaemonInstallation::Path) => (DaemonInstallation::Path, initial),
        None if explicit && has_fault(&initial, "RelayUnavailable") => {
            ensure!(
                expected_host.is_none(),
                "Corvid is missing on a previously configured host. Verify the connection before reinstalling"
            );
            let (binary, sha256) = read_bundle(&host, platform).await?;
            prompts
                .confirm(
                    Operation::Install,
                    host.name(),
                    "Install and start Corvid on this host.",
                )
                .await?;
            progress("Installing Corvid on the execution host…");
            host.install_daemon(platform, &binary, &sha256).await?;
            host = host.with_daemon(platform, DaemonInstallation::Managed);
            progress("Starting the Corvid daemon…");
            host.ensure_daemon(platform, DaemonInstallation::Managed)
                .await?;
            let link = host.interactive(capabilities).await?;
            return Ok((host, link));
        }
        _ => return Err(initial),
    };

    if !explicit {
        return Err(connection_error);
    }
    let stopped = has_fault(&connection_error, "DaemonUnavailable");
    let offered_upgrade = if stopped && daemon == DaemonInstallation::Managed {
        prepare_upgrade(&host, platform, daemon, expected_host)
            .await
            .ok()
    } else {
        None
    };
    let upgrade = match offered_upgrade {
        Some(upgrade) => upgrade,
        None => {
            let error = if stopped {
                progress("Starting the Corvid daemon…");
                match host.ensure_daemon(platform, daemon).await {
                    Ok(()) => match host.interactive(capabilities.clone()).await {
                        Ok(link) => return Ok((host, link)),
                        Err(error) => error,
                    },
                    Err(error) => error,
                }
            } else {
                connection_error
            };
            if !needs_upgrade(&error) {
                return Err(error);
            }
            match prepare_upgrade(&host, platform, daemon, expected_host).await {
                Ok(upgrade) => upgrade,
                Err(error) => {
                    prompts
                        .manual_upgrade(host.name(), &format!("{error:#}"))
                        .await;
                    return Err(error);
                }
            }
        }
    };
    prompts
        .confirm(Operation::Upgrade, host.name(), &upgrade_detail(&upgrade))
        .await?;
    apply_upgrade(&host, platform, &upgrade, &progress).await?;
    let link = host.interactive(capabilities).await?;
    Ok((host, link))
}

async fn apply_upgrade(
    host: &RemoteHost,
    platform: HostPlatform,
    upgrade: &Upgrade,
    progress: impl Fn(&str),
) -> Result<()> {
    progress("Upgrading the Corvid daemon on the execution host…");
    host.upgrade_managed_daemon(
        platform,
        &upgrade.binary,
        &upgrade.sha256,
        &upgrade.installed,
        upgrade.expected_host,
    )
    .await?;
    progress("Connecting to Corvid…");
    Ok(())
}

fn managed_platform(host: &RemoteHost) -> Option<HostPlatform> {
    match host {
        RemoteHost::Ssh(alias) => alias.daemon().and_then(|(platform, daemon)| {
            (daemon == DaemonInstallation::Managed).then_some(platform)
        }),
        RemoteHost::Wsl(distribution) => (distribution.daemon()
            == Some(DaemonInstallation::Managed))
        .then_some(HostPlatform::Linux),
    }
}

fn verify_host(actual: &str, expected: Option<&str>) -> Result<()> {
    ensure!(
        expected.is_none_or(|expected| expected == actual),
        ProtocolFault::new(
            "HostMismatch",
            "the connection points to a different Corvid host"
        )
    );
    Ok(())
}

fn upgrade_detail(upgrade: &Upgrade) -> String {
    format!(
        "Corvid {} → {}\n\nHosted shells and agents keep running.\nClients reconnect after the upgrade.\nNotification monitoring reconnects\nfor verified agents.",
        upgrade.installed,
        env!("CARGO_PKG_VERSION"),
    )
}

struct Upgrade {
    installed: String,
    binary: Vec<u8>,
    sha256: String,
    expected_host: Option<Uuid>,
}

async fn available_upgrade(
    host: &RemoteHost,
    platform: HostPlatform,
    host_id: &str,
) -> Result<Option<Upgrade>> {
    let installed = host
        .daemon_version(platform, DaemonInstallation::Managed)
        .await?;
    if !bundled_is_newer(&installed)? {
        return Ok(None);
    }
    load_upgrade(host, platform, installed, Some(host_id))
        .await
        .map(Some)
}

fn bundled_is_newer(installed: &str) -> Result<bool> {
    let current =
        Version::parse(installed).context("Could not verify the installed Corvid version")?;
    let bundled = Version::parse(env!("CARGO_PKG_VERSION"))?;
    Ok(current.cmp_precedence(&bundled).is_lt())
}

async fn prepare_upgrade(
    host: &RemoteHost,
    platform: HostPlatform,
    daemon: DaemonInstallation,
    expected_host: Option<&str>,
) -> Result<Upgrade> {
    ensure!(
        daemon == DaemonInstallation::Managed,
        "Update the Corvid installation on PATH using its existing installation method, then reconnect."
    );
    let installed = host.daemon_version(platform, daemon).await?;
    ensure!(
        bundled_is_newer(&installed)?,
        "The execution host has Corvid {installed} and this Desktop build is {}. Use a compatible Desktop build or update the execution host manually.",
        env!("CARGO_PKG_VERSION"),
    );
    load_upgrade(host, platform, installed, expected_host).await
}

async fn load_upgrade(
    host: &RemoteHost,
    platform: HostPlatform,
    installed: String,
    expected_host: Option<&str>,
) -> Result<Upgrade> {
    let (binary, sha256) = read_bundle(host, platform).await?;
    Ok(Upgrade {
        installed,
        binary,
        sha256,
        expected_host: expected_host.map(Uuid::parse_str).transpose()?,
    })
}

#[derive(Deserialize)]
struct Manifest {
    schema_version: u32,
    target: String,
    version: String,
    size: u64,
    sha256: String,
}

async fn read_bundle(host: &RemoteHost, platform: HostPlatform) -> Result<(Vec<u8>, String)> {
    let target = host.probe_daemon_target(platform).await?;
    let binary_name = match platform {
        HostPlatform::Windows => "corvid.exe",
        HostPlatform::Linux | HostPlatform::Macos => "corvid",
    };
    let exe = std::env::current_exe()?;
    let binaries = exe.parent().context("locating the Desktop installation")?;
    let resources = if cfg!(target_os = "macos") && binaries.ends_with("Contents/MacOS") {
        binaries
            .parent()
            .expect("MacOS has a parent")
            .join("Resources")
    } else {
        binaries.to_owned()
    };
    let directory = resources.join("daemon").join(target);
    let file = File::open(directory.join("manifest.json")).with_context(|| {
        format!("The {target} daemon bundle is unavailable. Extract the matching daemon bundle beside Corvid Desktop")
    })?;
    let mut manifest_bytes = Vec::new();
    file.take(MAX_MANIFEST + 1)
        .read_to_end(&mut manifest_bytes)?;
    ensure!(
        manifest_bytes.len() as u64 <= MAX_MANIFEST,
        "daemon manifest is too large"
    );
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).context("reading the daemon manifest")?;
    ensure!(
        manifest.schema_version == 1
            && manifest.target == target
            && (1..=MAX_BINARY).contains(&manifest.size),
        "invalid daemon bundle manifest"
    );
    ensure!(
        manifest.version == env!("CARGO_PKG_VERSION"),
        "The daemon bundle is version {}. Use the bundle for this Desktop build ({})",
        manifest.version,
        env!("CARGO_PKG_VERSION")
    );

    let file = File::open(directory.join(binary_name)).context("opening the bundled daemon")?;
    let mut binary = Vec::new();
    file.take(manifest.size + 1).read_to_end(&mut binary)?;
    ensure!(
        binary.len() as u64 == manifest.size,
        "bundled daemon size did not match"
    );
    let sha256 = hex::encode(Sha256::digest(&binary));
    ensure!(
        sha256 == manifest.sha256,
        "bundled daemon SHA-256 did not match"
    );
    Ok((binary, sha256))
}
