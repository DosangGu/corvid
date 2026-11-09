use anyhow::{Context, Result, bail, ensure};
use corvid_protocol::controller::{ACK_CAPABILITY, FOCUS_CAPABILITY, PAIR_CAPABILITY};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AcknowledgeAttention, ConnectionStatus, FocusAgentRequest, GetCapabilities,
    GetConnectionStatus, GetHost, HelloResult, HostInfo, RegisterClientBinding,
    UnregisterClientBinding,
};
use corvid_protocol::{CAPABILITY_QUERY, COMMAND_TIMEOUT, ProtocolFault};
use tokio::time::timeout;
use uuid::Uuid;

use crate::endpoint::{ControlConnection, ControlEndpoint};

#[cfg(target_os = "linux")]
mod local;
#[cfg(target_os = "linux")]
pub use local::{agents, clients, controller, host, ping, status, workspaces};

pub struct ControllerScope {
    pub controller_id: String,
    pub host_id: String,
    pub server_epoch: String,
}

pub enum ControllerRequest {
    Register {
        code: Uuid,
    },
    Unregister {
        client: Uuid,
        revision: u64,
    },
    Acknowledge {
        run: Uuid,
        attention: Uuid,
    },
    Focus {
        client: Uuid,
        run: Uuid,
        revision: u64,
    },
}

impl ControlEndpoint {
    pub async fn host(&self) -> Result<HostInfo> {
        match self.request(Body::GetHost(GetHost {}), "host.v1").await? {
            Body::HostInfo(host) => Ok(host),
            _ => bail!("unexpected response to GetHost"),
        }
    }

    pub async fn status(&self) -> Result<ConnectionStatus> {
        match self
            .request(
                Body::GetConnectionStatus(GetConnectionStatus {}),
                "connection_status.v1",
            )
            .await?
        {
            Body::ConnectionStatus(status) => Ok(status),
            _ => bail!("unexpected response to GetConnectionStatus"),
        }
    }

    async fn request(&self, body: Body, capability: &str) -> Result<Body> {
        let connection = self.connect(capability, None).await?;
        timeout(
            COMMAND_TIMEOUT,
            exchange(connection, body, Uuid::new_v4().to_string()),
        )
        .await
        .context("control command timed out")?
    }

    pub async fn capabilities(&self) -> Result<HelloResult> {
        let connection = self.connect(CAPABILITY_QUERY, None).await?;
        let expected = connection.wire.session.description();
        let response = timeout(
            COMMAND_TIMEOUT,
            exchange(
                connection,
                Body::GetCapabilities(GetCapabilities {}),
                Uuid::new_v4().to_string(),
            ),
        )
        .await
        .context("capability query timed out")??;
        let Body::Capabilities(description) = response else {
            bail!("unexpected response to GetCapabilities");
        };
        ensure!(
            description == expected,
            "capability response contradicts HelloResult"
        );
        Ok(description)
    }

    pub async fn controller(
        &self,
        scope: &ControllerScope,
        request_id: String,
        command: ControllerRequest,
    ) -> Result<Body> {
        let (capability, body) = command.body(scope.controller_id.clone());
        let connection = self
            .connect(capability, Some(scope.controller_id.clone()))
            .await?;
        if connection.wire.session.host_id != scope.host_id {
            return Err(ProtocolFault::new("HostMismatch", "daemon host has changed").into());
        }
        if connection.wire.session.server_epoch != scope.server_epoch {
            return Err(ProtocolFault::new("EpochMismatch", "daemon epoch has changed").into());
        }
        timeout(COMMAND_TIMEOUT, exchange(connection, body, request_id))
            .await
            .context("controller result timed out")?
    }
}

impl ControllerRequest {
    fn body(self, controller_id: String) -> (&'static str, Body) {
        match self {
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
            ControllerRequest::Focus {
                client,
                run,
                revision,
            } => (
                FOCUS_CAPABILITY,
                Body::FocusAgent(FocusAgentRequest {
                    controller_id,
                    client_id: client.to_string(),
                    agent_run_id: run.to_string(),
                    binding_revision: Some(revision),
                }),
            ),
        }
    }
}

async fn exchange(
    mut connection: ControlConnection,
    body: Body,
    request_id: String,
) -> Result<Body> {
    let wire = &mut connection.wire;
    let session = &wire.session;
    let request = session.envelope(Some(request_id), body);
    wire.writer.envelope(&request).await?;
    let reply = wire
        .reader
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
