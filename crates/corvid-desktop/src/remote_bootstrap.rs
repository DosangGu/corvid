use std::fs::File;
use std::io::Read;

use anyhow::{Context, Result, ensure};
use corvid_client::{DaemonInstallation, HostPlatform, InteractiveConnection, RemoteHost};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::daemon_prompt::{Operation, Prompts, has_fault, needs_upgrade};

const MAX_BINARY: u64 = 32 * 1024 * 1024;
const MAX_MANIFEST: u64 = 4096;

pub async fn connect(
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
            let (binary, sha256) = read_bundle(platform)?;
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
        .confirm(
            Operation::Upgrade,
            host.name(),
            &format!(
                "Corvid {} → {}\n\nHosted shells keep running.\nClients reconnect after the upgrade.",
                upgrade.installed,
                env!("CARGO_PKG_VERSION"),
            ),
        )
        .await?;
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
    let link = host.interactive(capabilities).await?;
    Ok((host, link))
}

struct Upgrade {
    installed: String,
    binary: Vec<u8>,
    sha256: String,
    expected_host: Option<Uuid>,
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
    let current =
        Version::parse(&installed).context("Could not verify the installed Corvid version")?;
    let bundled = Version::parse(env!("CARGO_PKG_VERSION"))?;
    ensure!(
        current.cmp_precedence(&bundled).is_lt(),
        "The execution host has Corvid {installed} and this Desktop build is {bundled}. Use a compatible Desktop build or update the execution host manually."
    );
    let (binary, sha256) = read_bundle(platform)?;
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

fn read_bundle(platform: HostPlatform) -> Result<(Vec<u8>, String)> {
    let (target, binary_name) = match platform {
        HostPlatform::Linux => ("x86_64-unknown-linux-musl", "corvid"),
        HostPlatform::Windows => ("x86_64-pc-windows-msvc", "corvid.exe"),
    };
    let exe = std::env::current_exe()?;
    let directory = exe
        .parent()
        .context("locating the Desktop installation")?
        .join("daemon")
        .join(target);
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
