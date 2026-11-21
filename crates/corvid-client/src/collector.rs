use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use corvid_protocol::collector::SHELL_AGENT_CAPABILITY;
use corvid_protocol::collector::{
    CAPABILITY, INPUT_BATCH_CAPABILITY, NESTED_SUBAGENT_CAPABILITY, RETAINED_AGENT_CAPABILITY,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use corvid_protocol::wire::RegisterShellAgent;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{CollectHook, Envelope, HookEvent};
use corvid_protocol::{PeerRole, ProtocolFault, Session};
use uuid::Uuid;

use crate::compio_connection::LocalStream as Stream;
use crate::connection::SocketRole;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(20);
const SUBMIT_TIMEOUT: Duration = Duration::from_millis(50);

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use compio::io::util::Splittable;
    use corvid_protocol::wire::{Envelope, HookCollected};
    use corvid_protocol::{
        CompioFrameReader as FrameReader, CompioFrameWriter as FrameWriter, FrameRole, accept_hello,
    };

    const HOST: &str = "00000000-0000-4000-8000-000000000001";
    const EPOCH: &str = "00000000-0000-4000-8000-000000000002";
    const PREVIOUS: &str = "00000000-0000-4000-8000-000000000003";

    async fn collect(
        capabilities: &[&str],
        host: &str,
        epoch: &str,
        body: Body,
    ) -> (Result<Body>, Option<Envelope>) {
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = Stream::from_std(client).unwrap();
        let server = Stream::from_std(server).unwrap();
        let server = async {
            let (read, write) = server.split();
            let mut writer = FrameWriter::start(write, corvid_protocol::FrameRole::Collector)
                .await
                .unwrap();
            let mut reader = FrameReader::start(read, FrameRole::Collector)
                .await
                .unwrap();
            let hello = reader.next_envelope().await.unwrap().unwrap();
            let (session, reply) =
                accept_hello(&hello, PeerRole::Collector, HOST, EPOCH, capabilities).unwrap();
            writer.envelope(&reply).await.unwrap();
            let request = reader.next_envelope().await.unwrap();
            if let Some(request) = &request {
                session.validate_request(request).unwrap();
                writer
                    .envelope(&session.envelope(
                        request.request_id.clone(),
                        Body::HookCollected(HookCollected {}),
                    ))
                    .await
                    .unwrap();
            }
            request
        };
        corvid_async::join!(
            exchange(client, host, epoch, vec![CAPABILITY.to_owned()], body),
            server
        )
    }

    #[compio::test]
    async fn old_daemons_keep_strict_epoch_fencing_and_current_hooks_work() {
        let body = || Body::CollectHook(CollectHook::default());
        let (result, request) = collect(&[CAPABILITY], HOST, EPOCH, body()).await;
        assert!(result.is_ok());
        assert!(
            matches!(request.unwrap().body, Some(Body::CollectHook(hook)) if hook.observer_epoch.is_none())
        );
        let (result, request) = collect(&[CAPABILITY], HOST, PREVIOUS, body()).await;
        assert!(result.is_err());
        assert!(request.is_none());
    }

    #[compio::test]
    async fn retained_hooks_send_the_launch_epoch_inside_a_current_envelope() {
        let capabilities = &[CAPABILITY, RETAINED_AGENT_CAPABILITY];
        let body = || Body::CollectHook(CollectHook::default());
        let (result, request) = collect(capabilities, HOST, PREVIOUS, body()).await;
        assert!(result.is_ok());
        let request = request.unwrap();
        assert_eq!(request.server_epoch.as_deref(), Some(EPOCH));
        assert!(
            matches!(request.body, Some(Body::CollectHook(hook)) if hook.observer_epoch.as_deref() == Some(PREVIOUS))
        );
        let (result, request) = collect(capabilities, PREVIOUS, PREVIOUS, body()).await;
        assert!(result.is_err());
        assert!(request.is_none());
        let (result, request) = collect(
            capabilities,
            HOST,
            PREVIOUS,
            Body::RegisterShellAgent(RegisterShellAgent::default()),
        )
        .await;
        assert!(result.is_err());
        assert!(request.is_none());
    }
}

fn hook_capabilities(observation: &CollectHook) -> Vec<String> {
    let mut capabilities = vec![CAPABILITY.to_owned()];
    match HookEvent::try_from(observation.event) {
        Ok(HookEvent::InputBatchFinished) => capabilities.push(INPUT_BATCH_CAPABILITY.to_owned()),
        Ok(HookEvent::NestedSubagentLinked) => {
            capabilities.push(NESTED_SUBAGENT_CAPABILITY.to_owned());
        }
        _ => {}
    }
    capabilities
}

fn check_hook_reply(body: Body) -> Result<()> {
    match body {
        Body::HookCollected(_) => Ok(()),
        _ => anyhow::bail!("unexpected collector response"),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn check_shell_reply(reply: Body) -> Result<Uuid> {
    match reply {
        Body::ShellAgentRegistered(reply) => Ok(Uuid::parse_str(&reply.launch_association)?),
        _ => anyhow::bail!("unexpected shell registration response"),
    }
}

async fn exchange(
    stream: Stream,
    host_id: &str,
    epoch: &str,
    capabilities: Vec<String>,
    body: Body,
) -> Result<Body> {
    let mut connection = crate::compio_connection::LocalConnection::establish(
        stream,
        PeerRole::Collector,
        capabilities,
        None,
    )
    .await?;
    let session = &connection.session;
    let request = prepare_request(session, host_id, epoch, body)?;
    connection.writer.envelope(&request).await?;
    let reply = connection
        .reader
        .next_envelope()
        .await?
        .context("collector closed")?;
    check_reply(session, &request, reply)
}

fn prepare_request(
    session: &Session,
    host_id: &str,
    epoch: &str,
    mut body: Body,
) -> Result<Envelope> {
    ensure!(session.host_id == host_id, "stale collector host");
    let retained_observer = matches!(body, Body::CollectHook(_))
        && session
            .capabilities
            .iter()
            .any(|capability| capability == RETAINED_AGENT_CAPABILITY);
    ensure!(
        session.server_epoch == epoch || retained_observer,
        "stale collector scope"
    );
    if retained_observer && let Body::CollectHook(hook) = &mut body {
        // The envelope always uses the current epoch. The daemon separately
        // authorizes the launch epoch against the retained process identity.
        hook.observer_epoch = Some(epoch.to_owned());
    }

    Ok(session.envelope(Some(Uuid::new_v4().to_string()), body))
}

fn check_reply(session: &Session, request: &Envelope, reply: Envelope) -> Result<Body> {
    session.validate_scope(&reply)?;
    ensure!(
        reply.request_id == request.request_id,
        "collector response ID mismatch"
    );
    match reply.body {
        Some(Body::Error(fault)) => Err(ProtocolFault::from_wire(&fault).into()),
        Some(body) => Ok(body),
        None => anyhow::bail!("missing collector response"),
    }
}

/// Submit an observer event on the current Compio runtime within the hook's deadline.
pub async fn submit_hook(
    runtime: &Path,
    host_id: &str,
    epoch: &str,
    observation: CollectHook,
    deadline: std::time::Instant,
) -> Result<()> {
    let time_left = deadline.saturating_duration_since(std::time::Instant::now());
    ensure!(!time_left.is_zero(), "hook deadline elapsed");
    let capabilities = hook_capabilities(&observation);
    let body = compio::time::timeout(
        time_left,
        compio_exchange(
            runtime,
            host_id,
            epoch,
            capabilities,
            Body::CollectHook(observation),
        ),
    )
    .await??;
    check_hook_reply(body)
}

/// Register a shell agent using the current Compio runtime.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub async fn register_shell_agent(
    runtime: &Path,
    host_id: &str,
    epoch: &str,
    registration: RegisterShellAgent,
) -> Result<Uuid> {
    let reply = compio_exchange(
        runtime,
        host_id,
        epoch,
        vec![SHELL_AGENT_CAPABILITY.to_owned()],
        Body::RegisterShellAgent(registration),
    )
    .await?;
    check_shell_reply(reply)
}

async fn compio_exchange(
    runtime: &Path,
    host_id: &str,
    epoch: &str,
    capabilities: Vec<String>,
    body: Body,
) -> Result<Body> {
    use crate::compio_connection;

    let stream = compio::time::timeout(
        CONNECT_TIMEOUT,
        compio_connection::open(runtime, SocketRole::Collector),
    )
    .await??;
    compio::time::timeout(
        SUBMIT_TIMEOUT,
        exchange(stream, host_id, epoch, capabilities, body),
    )
    .await?
}
