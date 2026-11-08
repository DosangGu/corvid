use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use corvid_protocol::collector::{
    CAPABILITY, HOST_ID, INPUT_BATCH_CAPABILITY, LAUNCH_ASSOCIATION, NESTED_SUBAGENT_CAPABILITY,
    RUNTIME_DIR, SERVER_EPOCH,
};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{CollectorTraffic, HookCollected};
use corvid_protocol::{PeerRole, ProtocolFault};
use portable_pty::CommandBuilder;
use tokio::net::UnixStream;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use crate::agents::{Hook, HookBinding};
use crate::control::handshake;
use crate::identity::Identity;
use crate::process::ProcessImage;
use crate::workspaces::Workspaces;

const CONNECTION_TIMEOUT: Duration = Duration::from_millis(75);

#[derive(Default)]
pub(crate) struct CollectorDiagnostics {
    accepted: AtomicU64,
    rejected: AtomicU64,
    failed: AtomicU64,
    saturated: AtomicU64,
}

impl CollectorDiagnostics {
    pub(crate) fn traffic(&self) -> CollectorTraffic {
        CollectorTraffic {
            accepted_submissions: self.accepted.load(Ordering::Relaxed),
            rejected_submissions: self.rejected.load(Ordering::Relaxed),
            connection_failures: self.failed.load(Ordering::Relaxed),
            saturated_connections: self.saturated.load(Ordering::Relaxed),
        }
    }

    fn submission(&self, accepted: bool) {
        increment(if accepted {
            &self.accepted
        } else {
            &self.rejected
        });
    }

    fn connection_failed(&self) {
        increment(&self.failed);
    }

    pub(crate) fn admission_refused(&self) {
        increment(&self.saturated);
    }
}

fn increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
        Some(count.saturating_add(1))
    });
}

pub(crate) struct HookEnvironment {
    pub(crate) runtime: PathBuf,
    pub(crate) identity: Arc<Identity>,
}

impl HookEnvironment {
    pub(crate) fn configure(&self, command: &mut CommandBuilder, association: Option<Uuid>) {
        for key in [LAUNCH_ASSOCIATION, HOST_ID, SERVER_EPOCH] {
            command.env_remove(key);
        }
        if let Some(association) = association {
            command.env(LAUNCH_ASSOCIATION, association.to_string());
            command.env(HOST_ID, &self.identity.host_id);
            command.env(SERVER_EPOCH, &self.identity.epoch);
            command.env(RUNTIME_DIR, &self.runtime);
        }
    }
}

pub(crate) async fn serve(
    stream: UnixStream,
    identity: &Identity,
    workspaces: &Workspaces,
) -> Result<()> {
    let outcome = timeout(CONNECTION_TIMEOUT, collect(stream, identity, workspaces)).await;
    if !matches!(&outcome, Ok(Ok(()))) {
        workspaces.collector.connection_failed();
    }
    outcome?
}

async fn collect(stream: UnixStream, identity: &Identity, workspaces: &Workspaces) -> Result<()> {
    let pid = stream
        .peer_cred()?
        .pid()
        .and_then(|pid| u32::try_from(pid).ok())
        .context("collector peer PID unavailable")?;
    let Some((session, mut reader, mut writer)) = handshake(
        stream,
        identity,
        PeerRole::Collector,
        &[
            CAPABILITY,
            INPUT_BATCH_CAPABILITY,
            NESTED_SUBAGENT_CAPABILITY,
        ],
    )
    .await?
    else {
        workspaces.collector.connection_failed();
        return Ok(());
    };
    let request = reader
        .next_envelope()
        .await?
        .context("missing hook submission")?;
    let result = match session.validate_request(&request) {
        Ok(Body::CollectHook(hook)) => match Hook::decode(hook, request.request_id.as_deref()) {
            Ok(hook) => ingest(workspaces, hook, pid).await,
            Err(fault) => Err(fault),
        },
        Err(fault) => Err(fault),
        Ok(_) => unreachable!("collector accepts only hook submissions"),
    };
    workspaces.collector.submission(result.is_ok());
    let body = match result {
        Ok(()) => Body::HookCollected(HookCollected {}),
        Err(fault) => Body::Error(fault.into_wire()),
    };
    writer
        .envelope(&session.envelope(request.request_id, body))
        .await?;
    Ok(())
}

async fn ingest(workspaces: &Workspaces, hook: Hook, pid: u32) -> Result<(), ProtocolFault> {
    let unauthorized = || ProtocolFault::new("Unauthorized", "hook has no verified launch owner");
    let owner = find_owner(workspaces, hook.association)
        .await
        .ok_or_else(unauthorized)?;
    let provider = hook.provider;
    let producer = tokio::task::spawn_blocking(move || owner.verify_peer(pid, provider))
        .await
        .ok()
        .flatten()
        .ok_or_else(unauthorized)?;
    workspaces.apply_hook(hook, owner, producer)
}

async fn find_owner(workspaces: &Workspaces, association: Uuid) -> Option<ProcessImage> {
    loop {
        match workspaces.hook_binding(association) {
            HookBinding::Owned(owner) => return Some(owner),
            HookBinding::Unavailable => return None,
            HookBinding::Starting => sleep(Duration::from_millis(2)).await,
        }
    }
}
