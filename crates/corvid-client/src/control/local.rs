use std::path::Path;

use anyhow::{Context, Result, bail};
use corvid_protocol::COMMAND_TIMEOUT;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AgentList, ClientList, ConnectionStatus, GetConnectionStatus, GetHost, HostInfo, ListAgents,
    ListClients, ListWorkspaces, Ping, WorkspaceList,
};
use tokio::time::timeout;
use uuid::Uuid;

use super::{ControlEndpoint, ControllerRequest, ControllerScope, exchange};

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

pub async fn controller(
    runtime: &Path,
    scope: &ControllerScope,
    request_id: String,
    command: ControllerRequest,
) -> Result<Body> {
    ControlEndpoint::Local(runtime.to_owned())
        .controller(scope, request_id, command)
        .await
}

async fn request(runtime: &Path, request: Body, capability: &str) -> Result<Body> {
    let connection = ControlEndpoint::Local(runtime.to_owned())
        .connect(capability, None)
        .await?;
    timeout(
        COMMAND_TIMEOUT,
        exchange(connection, request, Uuid::new_v4().to_string()),
    )
    .await
    .context("control command timed out")?
}
