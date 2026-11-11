use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use corvid_protocol::collector::{CAPABILITY, INPUT_BATCH_CAPABILITY};
use corvid_protocol::collector::{
    HOST_ID, LAUNCH_ASSOCIATION, RUNTIME_DIR, SERVER_EPOCH, SHELL_TERMINAL,
};
#[cfg(target_os = "linux")]
use corvid_protocol::collector::{NESTED_SUBAGENT_CAPABILITY, SHELL_AGENT_CAPABILITY};
use corvid_protocol::wire::CollectorTraffic;
use corvid_protocol::wire::HookCollected;
#[cfg(target_os = "linux")]
use corvid_protocol::wire::ShellAgentRegistered;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::{PeerRole, ProtocolFault};
use portable_pty::CommandBuilder;
#[cfg(target_os = "linux")]
use tokio::time::sleep;
use tokio::time::timeout;
use uuid::Uuid;

#[cfg(target_os = "linux")]
use crate::agents::HookBinding;
use crate::control::handshake;
use crate::hook_data::Hook;
use crate::identity::Identity;
use crate::platform::Stream;
#[cfg(target_os = "linux")]
use crate::process::ProcessImage;
#[cfg(target_os = "linux")]
use crate::provider::shell::Commands;
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
    pub(crate) fn configure(
        &self,
        command: &mut CommandBuilder,
        terminal: Option<Uuid>,
        _: Option<Uuid>,
    ) {
        for key in [
            LAUNCH_ASSOCIATION,
            HOST_ID,
            SERVER_EPOCH,
            SHELL_TERMINAL,
            RUNTIME_DIR,
        ] {
            command.env_remove(key);
        }
        if let Some(terminal) = terminal {
            command.env(LAUNCH_ASSOCIATION, terminal.to_string());
            command.env(HOST_ID, &self.identity.host_id);
            command.env(SERVER_EPOCH, &self.identity.epoch);
            command.env(RUNTIME_DIR, &self.runtime);
        }
    }
}

pub(crate) async fn serve(
    stream: Stream,
    identity: &Identity,
    workspaces: &Workspaces,
) -> Result<()> {
    let outcome = timeout(CONNECTION_TIMEOUT, collect(stream, identity, workspaces)).await;
    if !matches!(&outcome, Ok(Ok(()))) {
        workspaces.collector.connection_failed();
    }
    outcome?
}

async fn collect(stream: Stream, identity: &Identity, workspaces: &Workspaces) -> Result<()> {
    let pid = peer_pid(&stream).context("collector peer PID unavailable")?;
    let Some((session, mut reader, mut writer)) = handshake(
        stream,
        identity,
        PeerRole::Collector,
        &[
            CAPABILITY,
            INPUT_BATCH_CAPABILITY,
            #[cfg(target_os = "linux")]
            NESTED_SUBAGENT_CAPABILITY,
            #[cfg(target_os = "linux")]
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
        #[cfg(target_os = "linux")]
        Ok(Body::RegisterShellAgent(registration)) => workspaces
            .register_shell_agent(registration, pid)
            .await
            .map(|association| {
                Body::ShellAgentRegistered(ShellAgentRegistered {
                    launch_association: association.to_string(),
                })
            }),
        Err(fault) => Err(fault),
        Ok(_) => Err(ProtocolFault::new(
            "UnsupportedCapability",
            "collector request unavailable",
        )),
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

#[cfg(target_os = "linux")]
fn peer_pid(stream: &Stream) -> Option<u32> {
    stream.peer_cred().ok()?.pid()?.try_into().ok()
}

#[cfg(windows)]
fn peer_pid(stream: &Stream) -> Option<u32> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;

    let mut pid = 0;
    let read = unsafe { GetNamedPipeClientProcessId(stream.as_raw_handle(), &mut pid) };
    (read != 0 && pid != 0).then_some(pid)
}

#[cfg(windows)]
async fn ingest(workspaces: &Workspaces, hook: Hook, pid: u32) -> Result<(), ProtocolFault> {
    workspaces.apply_native_hook(hook, pid).await
}
