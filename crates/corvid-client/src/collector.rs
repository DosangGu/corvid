use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_protocol::collector::{CAPABILITY, INPUT_BATCH_CAPABILITY, NESTED_SUBAGENT_CAPABILITY};
use corvid_protocol::unix::SocketRole;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{CollectHook, HookEvent};
use corvid_protocol::{PeerRole, ProtocolFault};
use tokio::time::{Instant, timeout, timeout_at};
use uuid::Uuid;

use crate::connection::{UnixConnection, open};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(20);
const SUBMIT_TIMEOUT: Duration = Duration::from_millis(50);

pub async fn submit_hook(
    runtime: &Path,
    host_id: &str,
    epoch: &str,
    observation: CollectHook,
    deadline: Instant,
) -> Result<()> {
    let connect_deadline = deadline.min(Instant::now() + CONNECT_TIMEOUT);
    let stream = timeout_at(connect_deadline, open(runtime, SocketRole::Collector)).await??;
    timeout_at(
        deadline,
        timeout(
            SUBMIT_TIMEOUT,
            send_hook(stream, host_id, epoch, observation),
        ),
    )
    .await??
}

async fn send_hook(
    stream: tokio::net::UnixStream,
    host_id: &str,
    epoch: &str,
    observation: CollectHook,
) -> Result<()> {
    let mut capabilities = vec![CAPABILITY.to_owned()];
    match HookEvent::try_from(observation.event) {
        Ok(HookEvent::InputBatchFinished) => capabilities.push(INPUT_BATCH_CAPABILITY.to_owned()),
        Ok(HookEvent::NestedSubagentLinked) => {
            capabilities.push(NESTED_SUBAGENT_CAPABILITY.to_owned());
        }
        _ => {}
    }
    let mut connection =
        UnixConnection::establish(stream, PeerRole::Collector, capabilities).await?;
    let session = &connection.session;
    ensure!(
        session.host_id == host_id && session.server_epoch == epoch,
        "stale collector scope"
    );

    let request = session.envelope(
        Some(Uuid::new_v4().to_string()),
        Body::CollectHook(observation),
    );
    connection.writer.envelope(&request).await?;
    let reply = connection
        .reader
        .next_envelope()
        .await?
        .context("collector closed")?;
    session.validate_scope(&reply)?;
    ensure!(
        reply.request_id == request.request_id,
        "collector response ID mismatch"
    );
    match reply.body {
        Some(Body::HookCollected(_)) => Ok(()),
        Some(Body::Error(fault)) => Err(ProtocolFault::from_wire(&fault).into()),
        _ => anyhow::bail!("unexpected collector response"),
    }
}
