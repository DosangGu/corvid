use std::time::Duration;

use anyhow::{Context, Result};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    ConnectionStatus, Envelope, HostInfo, LayoutRecovery, PersistenceState, Pong, TabInfo,
    WorkspaceInfo, WorkspaceList,
};
use corvid_protocol::{
    COMMAND_TIMEOUT, FrameReader, FrameRole, FrameWriter, PROTOCOL_MAJOR, PROTOCOL_MINOR, PeerRole,
    ProtocolFault, Session, accept_hello,
};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::timeout;

use crate::identity::Identity;
use crate::metadata::{MetadataView, Recovery, SaveState};
use crate::workspaces::Workspaces;

const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const CAPABILITIES: &[&str] = &[
    "host.v1",
    "ping.v1",
    "connection_status.v1",
    "list.v1",
    "list_agents.v1",
];

pub(crate) async fn serve(
    stream: UnixStream,
    identity: &Identity,
    metadata: &MetadataView,
    workspaces: &Workspaces,
) -> Result<()> {
    let Some((session, mut reader, mut writer)) = timeout(
        COMMAND_TIMEOUT,
        handshake(stream, identity, PeerRole::Control, CAPABILITIES),
    )
    .await??
    else {
        return Ok(());
    };

    while let Some(request) = timeout(IDLE_TIMEOUT, reader.next_envelope()).await?? {
        let body = match session.validate_request(&request) {
            Ok(Body::Ping(_)) => Body::Pong(Pong {}),
            Ok(Body::GetHost(_)) => Body::HostInfo(HostInfo {
                host_id: identity.host_id.clone(),
                server_epoch: identity.epoch.clone(),
            }),
            Ok(Body::GetConnectionStatus(_)) => {
                Body::ConnectionStatus(status(metadata, workspaces.any_live()))
            }
            Ok(Body::ListWorkspaces(_)) => Body::WorkspaceList(inventory(metadata)),
            Ok(Body::ListAgents(_)) => Body::AgentList(workspaces.agents()),
            Ok(_) => Body::Error(
                ProtocolFault::new("UnsupportedCapability", "unavailable control method")
                    .into_wire(),
            ),
            Err(error) => Body::Error(error.into_wire()),
        };
        let reply = session.envelope(request.request_id, body);
        timeout(COMMAND_TIMEOUT, writer.envelope(&reply)).await??;
    }
    Ok(())
}

fn status(metadata: &MetadataView, terminal_live: bool) -> ConnectionStatus {
    let persistence = match metadata.save_state() {
        SaveState::Saved => PersistenceState::Saved,
        SaveState::Pending => PersistenceState::Pending,
        SaveState::Failed => PersistenceState::Failed,
    };
    let recovery = match metadata.recovery {
        Recovery::Normal => LayoutRecovery::Normal,
        Recovery::Backup => LayoutRecovery::Backup,
        Recovery::Fresh => LayoutRecovery::Fresh,
    };
    ConnectionStatus {
        terminal_live,
        persistence: persistence as i32,
        layout_recovery: recovery as i32,
    }
}

fn inventory(metadata: &MetadataView) -> WorkspaceList {
    let layout = metadata.layout();
    let workspaces = layout
        .workspaces
        .iter()
        .map(|workspace| WorkspaceInfo {
            workspace_id: workspace.id.to_string(),
            label: workspace.label.clone(),
            tabs: workspace
                .tabs
                .iter()
                .map(|tab| {
                    let mut pane_ids = Vec::new();
                    tab.root.pane_ids(&mut pane_ids);
                    TabInfo {
                        tab_id: tab.id.to_string(),
                        label: tab.label.clone(),
                        pane_ids,
                    }
                })
                .collect(),
        })
        .collect();
    WorkspaceList { workspaces }
}

type Connection = (
    Session,
    FrameReader<OwnedReadHalf>,
    FrameWriter<OwnedWriteHalf>,
);

pub(crate) async fn handshake(
    stream: UnixStream,
    identity: &Identity,
    role: PeerRole,
    capabilities: &[&str],
) -> Result<Option<Connection>> {
    let (read, write) = stream.into_split();
    let output = match role {
        PeerRole::Control => FrameRole::StructuredOnly,
        PeerRole::Tui => FrameRole::TuiOutput,
    };
    let mut writer = FrameWriter::start(write, output).await?;
    let mut reader = FrameReader::start(read, FrameRole::StructuredOnly).await?;
    let request = reader
        .next_envelope()
        .await?
        .context("missing control Hello")?;
    match accept_hello(
        &request,
        role,
        &identity.host_id,
        &identity.epoch,
        capabilities,
    ) {
        Ok((session, reply)) => {
            writer.envelope(&reply).await?;
            Ok(Some((session, reader, writer)))
        }
        Err(error) => {
            writer
                .envelope(&bootstrap_error(request.request_id, error))
                .await?;
            Ok(None)
        }
    }
}

fn bootstrap_error(request_id: Option<String>, error: ProtocolFault) -> Envelope {
    Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        request_id: request_id.filter(|id| id.len() <= 256 && !id.chars().any(char::is_control)),
        body: Some(Body::Error(error.into_wire())),
        ..Default::default()
    }
}
