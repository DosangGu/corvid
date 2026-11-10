#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use anyhow::{Context, Result};
#[cfg(target_os = "linux")]
use corvid_protocol::collector::{
    CAPABILITY, INPUT_BATCH_CAPABILITY, NESTED_SUBAGENT_CAPABILITY, SHELL_AGENT_CAPABILITY,
};
use corvid_protocol::collector::{
    HOST_ID, LAUNCH_ASSOCIATION, RUNTIME_DIR, SERVER_EPOCH, SHELL_TERMINAL,
};
use corvid_protocol::wire::CollectorTraffic;
#[cfg(target_os = "linux")]
use corvid_protocol::wire::envelope::Body;
#[cfg(target_os = "linux")]
use corvid_protocol::wire::{HookCollected, ShellAgentRegistered};
#[cfg(target_os = "linux")]
use corvid_protocol::{PeerRole, ProtocolFault};
use portable_pty::CommandBuilder;
#[cfg(target_os = "linux")]
use tokio::net::UnixStream;
#[cfg(target_os = "linux")]
use tokio::time::{sleep, timeout};
use uuid::Uuid;

#[cfg(target_os = "linux")]
use crate::agents::{Hook, HookBinding};
#[cfg(target_os = "linux")]
use crate::control::handshake;
#[cfg(target_os = "linux")]
use crate::identity::Identity;
#[cfg(target_os = "linux")]
use crate::process::ProcessImage;
#[cfg(target_os = "linux")]
use crate::provider::shell::Commands;
#[cfg(target_os = "linux")]
use crate::workspaces::Workspaces;

#[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    fn submission(&self, accepted: bool) {
        increment(if accepted {
            &self.accepted
        } else {
            &self.rejected
        });
    }

    #[cfg(target_os = "linux")]
    fn connection_failed(&self) {
        increment(&self.failed);
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn admission_refused(&self) {
        increment(&self.saturated);
    }
}

#[cfg(target_os = "linux")]
fn increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
        Some(count.saturating_add(1))
    });
}

pub(crate) struct HookEnvironment {
    #[cfg(target_os = "linux")]
    pub(crate) runtime: PathBuf,
    #[cfg(target_os = "linux")]
    pub(crate) identity: Arc<Identity>,
    #[cfg(target_os = "linux")]
    pub(crate) shell_commands: Option<Commands>,
}

impl HookEnvironment {
    #[cfg(target_os = "linux")]
    pub(crate) fn configure(
        &self,
        command: &mut CommandBuilder,
        association: Option<Uuid>,
        shell_terminal: Option<Uuid>,
    ) {
        for key in [LAUNCH_ASSOCIATION, HOST_ID, SERVER_EPOCH, SHELL_TERMINAL] {
            command.env_remove(key);
        }
        let shell_terminal = shell_terminal.filter(|_| {
            self.shell_commands
                .as_ref()
                .is_some_and(|commands| commands.configure(command))
        });
        if let Some(terminal) = shell_terminal {
            command.env(SHELL_TERMINAL, terminal.to_string());
        }
        if let Some(association) = association {
            command.env(LAUNCH_ASSOCIATION, association.to_string());
        }
        if association.is_some() || shell_terminal.is_some() {
            command.env(HOST_ID, &self.identity.host_id);
            command.env(SERVER_EPOCH, &self.identity.epoch);
            command.env(RUNTIME_DIR, &self.runtime);
        }
    }
    #[cfg(windows)]
    pub(crate) fn configure(&self, command: &mut CommandBuilder, _: Option<Uuid>, _: Option<Uuid>) {
        for key in [
            LAUNCH_ASSOCIATION,
            HOST_ID,
            SERVER_EPOCH,
            SHELL_TERMINAL,
            RUNTIME_DIR,
        ] {
            command.env_remove(key);
        }
    }
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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
            SHELL_AGENT_CAPABILITY,
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
        Ok(Body::CollectHook(hook)) => {
            let result = match Hook::decode(hook, request.request_id.as_deref()) {
                Ok(hook) => ingest(workspaces, hook, pid).await,
                Err(fault) => Err(fault),
            };
            workspaces.collector.submission(result.is_ok());
            result.map(|()| Body::HookCollected(HookCollected {}))
        }
        Ok(Body::RegisterShellAgent(registration)) => workspaces
            .register_shell_agent(registration, pid)
            .await
            .map(|association| {
                Body::ShellAgentRegistered(ShellAgentRegistered {
                    launch_association: association.to_string(),
                })
            }),
        Err(fault) => Err(fault),
        Ok(_) => unreachable!("collector accepts only hook submissions"),
    };
    let body = match result {
        Ok(body) => body,
        Err(fault) => Body::Error(fault.into_wire()),
    };
    writer
        .envelope(&session.envelope(request.request_id, body))
        .await?;
    Ok(())
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
async fn find_owner(workspaces: &Workspaces, association: Uuid) -> Option<ProcessImage> {
    loop {
        match workspaces.hook_binding(association) {
            HookBinding::Owned(owner) => return Some(owner),
            HookBinding::Unavailable => return None,
            HookBinding::Starting => sleep(Duration::from_millis(2)).await,
        }
    }
}
