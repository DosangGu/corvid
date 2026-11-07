use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_protocol::PeerRole;
use corvid_protocol::collector::CAPABILITY;
use corvid_protocol::unix::SocketRole;
use corvid_protocol::wire::CollectHook;
use corvid_protocol::wire::envelope::Body;
use tokio::time::{Instant, timeout, timeout_at};
use uuid::Uuid;

use crate::connection::{Connection, open};

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
    let mut connection =
        Connection::establish(stream, PeerRole::Collector, vec![CAPABILITY.to_owned()]).await?;
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
        reply.request_id == request.request_id
            && matches!(reply.body, Some(Body::HookCollected(_))),
        "collector rejected observation"
    );
    Ok(())
}
