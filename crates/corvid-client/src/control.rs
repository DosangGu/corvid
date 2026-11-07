use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use corvid_protocol::controller::{ACK_CAPABILITY, PAIR_CAPABILITY};
use corvid_protocol::unix::SocketRole;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AcknowledgeAttention, AgentList, ClientList, ConnectionStatus, GetConnectionStatus, GetHost,
    HostInfo, ListAgents, ListClients, ListWorkspaces, Ping, RegisterClientBinding,
    UnregisterClientBinding, WorkspaceList,
};
use corvid_protocol::{COMMAND_TIMEOUT, PeerRole, ProtocolFault};
use tokio::time::timeout;
use uuid::Uuid;

use crate::connection::{Connection, open};

pub async fn host(runtime: &Path) -> Result<HostInfo> {
    match request(runtime, Body::GetHost(GetHost {}), "host.v1").await? {
        Body::HostInfo(host) => Ok(host),
        _ => bail!("unexpected response to GetHost"),
    }
}

pub async fn ping(runtime: &Path) -> Result<()> {
    match request(runtime, Body::Ping(Ping {}), "ping.v1").await? {
        Body::Pong(_) => Ok(()),
        _ => bail!("unexpected response to Ping"),
    }
}

pub async fn status(runtime: &Path) -> Result<ConnectionStatus> {
    match request(
        runtime,
        Body::GetConnectionStatus(GetConnectionStatus {}),
        "connection_status.v1",
    )
    .await?
    {
        Body::ConnectionStatus(status) => Ok(status),
        _ => bail!("unexpected response to GetConnectionStatus"),
    }
}

pub async fn workspaces(runtime: &Path) -> Result<WorkspaceList> {
    match request(runtime, Body::ListWorkspaces(ListWorkspaces {}), "list.v1").await? {
        Body::WorkspaceList(workspaces) => Ok(workspaces),
        _ => bail!("unexpected response to ListWorkspaces"),
    }
}

pub async fn agents(runtime: &Path) -> Result<AgentList> {
    match request(runtime, Body::ListAgents(ListAgents {}), "list_agents.v1").await? {
        Body::AgentList(agents) => Ok(agents),
        _ => bail!("unexpected response to ListAgents"),
    }
}

pub async fn clients(runtime: &Path) -> Result<ClientList> {
    match request(
        runtime,
        Body::ListClients(ListClients {}),
        "list_clients.v1",
    )
    .await?
    {
        Body::ClientList(clients) => Ok(clients),
        _ => bail!("unexpected response to ListClients"),
    }
}

pub struct ControllerScope {
    pub controller_id: String,
    pub host_id: String,
    pub server_epoch: String,
}

pub enum ControllerRequest {
    Register { code: Uuid },
    Unregister { client: Uuid, revision: u64 },
    Acknowledge { run: Uuid, attention: Uuid },
}

pub async fn controller(
    runtime: &Path,
    scope: &ControllerScope,
    request_id: String,
    command: ControllerRequest,
) -> Result<Body> {
    let controller_id = scope.controller_id.clone();
    let (capability, body) = match command {
        ControllerRequest::Register { code } => (
            PAIR_CAPABILITY,
            Body::RegisterClientBinding(RegisterClientBinding {
                controller_id,
                code: code.to_string(),
            }),
        ),
        ControllerRequest::Unregister { client, revision } => (
            PAIR_CAPABILITY,
            Body::UnregisterClientBinding(UnregisterClientBinding {
                controller_id,
                client_id: client.to_string(),
                binding_revision: Some(revision),
            }),
        ),
        ControllerRequest::Acknowledge { run, attention } => (
            ACK_CAPABILITY,
            Body::AcknowledgeAttention(AcknowledgeAttention {
                controller_id,
                agent_run_id: run.to_string(),
                attention_id: attention.to_string(),
            }),
        ),
    };
    let stream = open(runtime, SocketRole::Control).await?;
    let connection = Connection::establish_as(
        stream,
        PeerRole::Control,
        vec![capability.to_owned()],
        Some(scope.controller_id.clone()),
    )
    .await?;
    if connection.session.host_id != scope.host_id {
        return Err(ProtocolFault::new("HostMismatch", "daemon host has changed").into());
    }
    if connection.session.server_epoch != scope.server_epoch {
        return Err(ProtocolFault::new("EpochMismatch", "daemon epoch has changed").into());
    }
    timeout(COMMAND_TIMEOUT, exchange(connection, body, request_id))
        .await
        .context("controller result timed out")?
}

async fn request(runtime: &Path, request: Body, capability: &str) -> Result<Body> {
    let stream = open(runtime, SocketRole::Control).await?;
    let connection =
        Connection::establish(stream, PeerRole::Control, vec![capability.to_owned()]).await?;
    timeout(
        COMMAND_TIMEOUT,
        exchange(connection, request, Uuid::new_v4().to_string()),
    )
    .await
    .context("control command timed out")?
}

async fn exchange(connection: Connection, body: Body, request_id: String) -> Result<Body> {
    let Connection {
        session,
        mut reader,
        mut writer,
    } = connection;
    let request = session.envelope(Some(request_id), body);
    writer.envelope(&request).await?;
    let reply = reader
        .next_envelope()
        .await?
        .context("daemon closed during command")?;
    session.validate_scope(&reply)?;
    ensure!(
        request.request_id == reply.request_id,
        "control request ID mismatch"
    );
    match reply.body.context("unknown response body")? {
        Body::Error(error) => Err(ProtocolFault::from_wire(&error).into()),
        Body::HostInfo(host)
            if host.host_id != session.host_id || host.server_epoch != session.server_epoch =>
        {
            bail!("host response contradicts negotiated daemon identity")
        }
        body => Ok(body),
    }
}
