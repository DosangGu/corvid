use std::fs::File;
use std::io::Read;

use anyhow::{Context, Result, ensure};
use corvid_client::{InteractiveConnection, SshAlias, SshDaemon};
use corvid_protocol::ProtocolFault;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const LINUX_TARGET: &str = "x86_64-unknown-linux-musl";
const MAX_BINARY: u64 = 32 * 1024 * 1024;
const MAX_MANIFEST: u64 = 4096;

pub async fn connect(
    alias: SshAlias,
    capabilities: Vec<String>,
    explicit: bool,
    expected_host: Option<&str>,
    progress: impl Fn(&str),
) -> Result<(SshAlias, InteractiveConnection)> {
    let mut alias = alias.with_daemon(SshDaemon::Path);
    let initial = match alias.interactive(capabilities.clone()).await {
        Ok(link) => return Ok((alias, link)),
        Err(error) => error,
    };
    if !has_fault(&initial, "RelayUnavailable") && !has_fault(&initial, "DaemonUnavailable") {
        return Err(initial);
    }

    progress("Checking the remote Corvid installation…");
    let daemon = match alias.probe_daemon().await? {
        Some(SshDaemon::Managed) => {
            alias = alias.with_daemon(SshDaemon::Managed);
            match alias.interactive(capabilities.clone()).await {
                Ok(link) => return Ok((alias, link)),
                Err(error) if !explicit || !has_fault(&error, "DaemonUnavailable") => {
                    return Err(error);
                }
                Err(_) => SshDaemon::Managed,
            }
        }
        Some(SshDaemon::Path) if explicit => SshDaemon::Path,
        None if explicit => {
            ensure!(
                expected_host.is_none(),
                "Corvid is missing on a previously configured host. Verify the SSH alias before reinstalling"
            );
            let (binary, sha256) = read_bundle()?;
            progress("Installing Corvid on the SSH host…");
            alias.install_daemon(&binary, &sha256).await?;
            alias = alias.with_daemon(SshDaemon::Managed);
            SshDaemon::Managed
        }
        _ => return Err(initial),
    };

    progress("Starting the Corvid daemon…");
    alias.ensure_daemon(daemon).await?;
    progress("Connecting to Corvid…");
    let link = alias.interactive(capabilities).await?;
    Ok((alias, link))
}

fn has_fault(error: &anyhow::Error, code: &str) -> bool {
    error
        .downcast_ref::<ProtocolFault>()
        .is_some_and(|fault| fault.code == code)
}

#[derive(Deserialize)]
struct Manifest {
    schema_version: u32,
    target: String,
    size: u64,
    sha256: String,
}

fn read_bundle() -> Result<(Vec<u8>, String)> {
    let exe = std::env::current_exe()?;
    let directory = exe
        .parent()
        .context("locating the Desktop installation")?
        .join("daemon")
        .join(LINUX_TARGET);
    let file = File::open(directory.join("manifest.json")).context(
        "Linux daemon bundle is unavailable. Extract Corvid Desktop from the package with the daemon",
    )?;
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
            && manifest.target == LINUX_TARGET
            && (1..=MAX_BINARY).contains(&manifest.size),
        "invalid Linux daemon bundle manifest"
    );

    let file = File::open(directory.join("corvid")).context("opening the bundled Linux daemon")?;
    let mut binary = Vec::new();
    file.take(manifest.size + 1).read_to_end(&mut binary)?;
    ensure!(
        binary.len() as u64 == manifest.size,
        "bundled Linux daemon size did not match"
    );
    let sha256 = format!("{:x}", Sha256::digest(&binary));
    ensure!(
        sha256 == manifest.sha256,
        "bundled Linux daemon SHA-256 did not match"
    );
    Ok((binary, sha256))
}
