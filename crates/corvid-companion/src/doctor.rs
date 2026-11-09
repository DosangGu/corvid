use std::io;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_client::check_system_openssh;
use corvid_protocol::ProtocolFault;
use corvid_protocol::wire::{CompanionConnection, PresentationSlots};
use tokio::time::timeout;
use uuid::Uuid;

use crate::platform::{TerminalRegistry, WezTermPane};
use crate::{Config, PresentationClient};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckStatus {
    Pass,
    Fail,
    Skip,
}

pub struct Check {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
}

pub async fn diagnose(instance: &str, state: &Path) -> Vec<Check> {
    use CheckStatus::{Fail, Pass, Skip};

    let openssh = check_system_openssh().is_ok();
    let config = Config::read(state);
    let presentation = timeout(
        Duration::from_secs(4),
        read_presentation(instance, config.as_ref().ok()),
    )
    .await;
    let ready = presentation.as_ref().ok().and_then(|result| {
        result
            .as_ref()
            .ok()
            .filter(|slots| slots.connection == CompanionConnection::Ready as i32)
    });

    let mut checks = vec![
        if openssh {
            check("OpenSSH", Pass, "System OpenSSH client found.")
        } else {
            check(
                "OpenSSH",
                Fail,
                "Enable the Windows OpenSSH Client feature.",
            )
        },
        if config.is_ok() {
            check(
                "State",
                Pass,
                "Private state directory and configuration verified.",
            )
        } else {
            check(
                "State",
                Fail,
                "Cannot read private companion configuration. Check --state-dir and its ACL, or run companion init for a new instance.",
            )
        },
        match &presentation {
            Ok(Ok(_)) => check(
                "Named pipe",
                Pass,
                "Verified and queried the companion pipe.",
            ),
            Ok(Err(error))
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                check(
                    "Named pipe",
                    Fail,
                    "Companion pipe not found. Check --pipe-instance and start companion run.",
                )
            }
            Ok(Err(_)) => check(
                "Named pipe",
                Fail,
                "Could not verify the pipe or its companion identity. Check its ACL and instance settings.",
            ),
            Err(_) => check("Named pipe", Fail, "Companion pipe query timed out."),
        },
    ];
    checks.push(match (&config, openssh, ready) {
        (Ok(_), _, Some(_)) => check(
            "Remote host",
            Pass,
            "Companion is synchronized with the configured host.",
        ),
        (Ok(config), true, None) => probe_remote(config).await,
        _ => check(
            "Remote host",
            Skip,
            "Requires private configuration and system OpenSSH.",
        ),
    });
    checks.push(match (&config, ready) {
        (Ok(_), Some(slots)) if slots.bindings.is_empty() => {
            check("WezTerm", Skip, "No attached terminal is paired.")
        }
        (Ok(config), Some(slots)) => match timeout(Duration::from_secs(5), verify_bindings(config, slots)).await {
            Ok(Ok((verified, manual, unavailable))) => check(
                "WezTerm",
                if unavailable == 0 { Pass } else { Fail },
                format!("{verified} verified WezTerm, {manual} manual, {unavailable} unavailable. Pair any unavailable terminal again."),
            ),
            _ => check("WezTerm", Fail, "Could not verify saved registrations. Refresh the board and pair the intended terminal again."),
        },
        _ => check("WezTerm", Skip, "Requires a synchronized companion with matching configuration."),
    });
    checks
}

fn check(name: &'static str, status: CheckStatus, detail: impl Into<String>) -> Check {
    Check {
        name,
        status,
        detail: detail.into(),
    }
}

async fn read_presentation(instance: &str, config: Option<&Config>) -> Result<PresentationSlots> {
    PresentationClient::connect(instance)
        .await?
        .slots(config)
        .await
}

async fn probe_remote(config: &Config) -> Check {
    let probe = timeout(Duration::from_secs(15), async {
        let endpoint = config.control_endpoint()?;
        let watch = endpoint
            .watch(
                &config.controller_id.to_string(),
                &config.host_id.to_string(),
                || {},
            )
            .await?;
        ensure!(watch.is_ready(), "snapshot watch is unavailable");
        Ok::<(), anyhow::Error>(())
    })
    .await;
    match probe {
        Ok(Ok(())) => check(
            "Remote host",
            CheckStatus::Pass,
            "Connected to the configured host and synchronized a test snapshot.",
        ),
        Ok(Err(error)) => check(
            "Remote host",
            CheckStatus::Fail,
            remote_hint(
                error
                    .downcast_ref::<ProtocolFault>()
                    .map(|fault| fault.code.as_str()),
            ),
        ),
        Err(_) => check(
            "Remote host",
            CheckStatus::Fail,
            "Remote SSH or snapshot setup timed out.",
        ),
    }
}

fn remote_hint(code: Option<&str>) -> &'static str {
    match code {
        Some("HostKeyRequired") => "Verify the host key using Windows OpenSSH.",
        Some("AuthenticationRequired") => {
            "Configure noninteractive Windows OpenSSH authentication."
        }
        Some("RelayUnavailable") => "Install corvid on the remote command PATH.",
        Some("DaemonUnavailable") => "Start the configured Linux Corvid daemon.",
        Some("HostMismatch") => "The SSH alias reached a different daemon host ID.",
        Some("ProtocolMismatch" | "UnsupportedCapability") => {
            "The remote daemon does not support the required protocol or snapshot watch."
        }
        _ => "Could not synchronize the remote daemon. Check the SSH alias and daemon status.",
    }
}

async fn verify_bindings(
    config: &Config,
    slots: &PresentationSlots,
) -> Result<(usize, usize, usize)> {
    let epoch = Uuid::parse_str(
        slots
            .remote_epoch
            .as_deref()
            .context("missing remote epoch")?,
    )?;
    let sequence = slots.remote_sequence.context("missing remote sequence")?;
    let records = TerminalRegistry::new(config.controller_id, config.host_id)?.load()?;
    let (mut verified, mut manual, mut unavailable) = (0, 0, 0);
    for binding in &slots.bindings {
        let client = Uuid::parse_str(&binding.client_id)?;
        let record = records
            .get(&client)
            .filter(|record| record.matches(epoch, binding.revision, sequence));
        match record {
            Some(record) => match &record.pane {
                None => manual += 1,
                Some(pane) => {
                    if WezTermPane::restore(pane).await.is_ok() {
                        verified += 1;
                    } else {
                        unavailable += 1;
                    }
                }
            },
            None => unavailable += 1,
        }
    }
    Ok((verified, manual, unavailable))
}
