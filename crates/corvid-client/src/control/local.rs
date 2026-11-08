use std::path::Path;

use anyhow::{Result, bail};
use corvid_protocol::ProtocolFault;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::snapshot_entity::Body as EntityBody;
use corvid_protocol::wire::{
    AgentList, ClientList, ConnectionStatus, GetHost, HostInfo, ListAgents, ListClients,
    ListWorkspaces, Ping, WorkspaceList,
};

use super::{ControlEndpoint, ControllerRequest, ControllerScope};

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
    ControlEndpoint::Local(runtime.to_owned()).status().await
}

pub async fn workspaces(runtime: &Path) -> Result<WorkspaceList> {
    match request(runtime, Body::ListWorkspaces(ListWorkspaces {}), "list.v1").await? {
        Body::WorkspaceList(workspaces) => Ok(workspaces),
        _ => bail!("unexpected response to ListWorkspaces"),
    }
}

pub async fn agents(runtime: &Path) -> Result<AgentList> {
    match request(runtime, Body::ListAgents(ListAgents {}), "list_agents.v1").await {
        Ok(Body::AgentList(agents)) => return Ok(agents),
        Ok(_) => bail!("unexpected response to ListAgents"),
        Err(error)
            if error
                .downcast_ref::<ProtocolFault>()
                .is_some_and(|fault| fault.code == "InventoryTooLarge") => {}
        Err(error) => return Err(error),
    }

    let snapshot = crate::snapshot(runtime).await?;
    let agents = snapshot
        .snapshot()
        .entities()
        .filter_map(|entity| match &entity.body {
            Some(EntityBody::Agent(agent)) => agent.summary.clone(),
            _ => None,
        })
        .collect();
    Ok(AgentList { agents })
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
    ControlEndpoint::Local(runtime.to_owned())
        .request(request, capability)
        .await
}
