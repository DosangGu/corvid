use std::time::Duration;

use anyhow::{Context, Result};
use corvid_protocol::controller::{ACK_CAPABILITY, FOCUS_CAPABILITY, PAIR_CAPABILITY};
use corvid_protocol::snapshot::{HISTORY_CAPABILITY, SNAPSHOT_CAPABILITY, WATCH_CAPABILITY};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    ConnectionStatus, Envelope, HostInfo, LayoutRecovery, PersistenceState, Pong, TabInfo,
    WorkspaceInfo, WorkspaceList,
};
use corvid_protocol::{
    CAPABILITY_QUERY, COMMAND_TIMEOUT, CONTROL_LIMIT, FrameReader, FrameRole, FrameWriter,
    PROTOCOL_MAJOR, PROTOCOL_MINOR, PeerRole, ProtocolFault, Session, accept_hello_with_profiles,
};
use prost::Message;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::{Instant, sleep_until, timeout};

use crate::identity::Identity;
use crate::incoming::Incoming;
use crate::metadata::{MetadataView, Recovery, SaveState};
use crate::provider::qualified_profiles;
use crate::snapshot::{self, LiveWatch};
use crate::workspaces::{ControllerCommand, Workspaces};

const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const CAPABILITIES: &[&str] = &[
    "host.v1",
    CAPABILITY_QUERY,
    "ping.v1",
    "connection_status.v1",
    "list.v1",
    "list_agents.v1",
    "list_clients.v1",
    SNAPSHOT_CAPABILITY,
    WATCH_CAPABILITY,
    HISTORY_CAPABILITY,
    PAIR_CAPABILITY,
    ACK_CAPABILITY,
    FOCUS_CAPABILITY,
];

pub(crate) async fn serve(
    stream: UnixStream,
    identity: &Identity,
    metadata: &MetadataView,
    workspaces: &Workspaces,
) -> Result<()> {
    let Some((session, reader, writer)) = timeout(
        COMMAND_TIMEOUT,
        handshake(stream, identity, PeerRole::Control, CAPABILITIES),
    )
    .await??
    else {
        return Ok(());
    };

    Control {
        session,
        writer,
        metadata,
        workspaces,
    }
    .run(reader)
    .await
}

struct Control<'a> {
    session: Session,
    writer: FrameWriter<OwnedWriteHalf>,
    metadata: &'a MetadataView,
    workspaces: &'a Workspaces,
}

impl Control<'_> {
    async fn run(mut self, mut reader: FrameReader<OwnedReadHalf>) -> Result<()> {
        while let Some(request) = timeout(IDLE_TIMEOUT, reader.next_envelope()).await?? {
            let (watch, include_history) = match self.session.validate_request(&request) {
                Ok(Body::GetSnapshot(options)) => (false, options.include_history),
                Ok(Body::SnapshotWatch(options)) => (true, options.include_history),
                _ => {
                    self.respond(request).await?;
                    continue;
                }
            };
            match self.workspaces.snapshot(watch) {
                Ok(capture) => {
                    let history = include_history.then(|| self.metadata.history.clone());
                    let live = snapshot::transfer(
                        capture,
                        history,
                        request.request_id,
                        &self.session,
                        &mut self.writer,
                        self.workspaces,
                    )
                    .await?;
                    if let Some(live) = live {
                        return self.watch(reader, live).await;
                    }
                }
                Err(fault) => {
                    self.reply(request.request_id, Body::Error(fault.into_wire()))
                        .await?
                }
            }
        }
        Ok(())
    }

    async fn respond(&mut self, request: Envelope) -> Result<bool> {
        let validated = self.session.validate_request(&request);
        let valid = validated.is_ok();
        let body = match validated {
            Ok(Body::Ping(_)) => Body::Pong(Pong {}),
            Ok(Body::GetHost(_)) => Body::HostInfo(HostInfo {
                host_id: self.session.host_id.clone(),
                server_epoch: self.session.server_epoch.clone(),
            }),
            Ok(Body::GetCapabilities(_)) => Body::Capabilities(self.session.description()),
            Ok(Body::GetConnectionStatus(_)) => {
                Body::ConnectionStatus(status(self.metadata, self.workspaces))
            }
            Ok(Body::ListWorkspaces(_)) => Body::WorkspaceList(inventory(self.metadata)),
            Ok(Body::ListAgents(_)) => Body::AgentList(self.workspaces.agents()),
            Ok(Body::ListClients(_)) => Body::ClientList(self.workspaces.clients()),
            Ok(
                body @ (Body::RegisterClientBinding(_)
                | Body::UnregisterClientBinding(_)
                | Body::AcknowledgeAttention(_)
                | Body::FocusAgent(_)),
            ) => match self.controller(&request, body).await {
                Ok(body) => body,
                Err(fault) => Body::Error(fault.into_wire()),
            },
            Ok(_) => Body::Error(
                ProtocolFault::new("UnsupportedCapability", "unavailable control method")
                    .into_wire(),
            ),
            Err(error) => Body::Error(error.into_wire()),
        };
        self.reply(request.request_id, body).await?;
        Ok(valid)
    }

    async fn reply(&mut self, request_id: Option<String>, body: Body) -> Result<()> {
        let mut reply = self.session.envelope(request_id, body);
        if matches!(&reply.body, Some(Body::AgentList(_))) && reply.encoded_len() >= CONTROL_LIMIT {
            let fault = ProtocolFault::new("InventoryTooLarge", "read agents from a snapshot");
            reply.body = Some(Body::Error(fault.into_wire()));
        }
        timeout(COMMAND_TIMEOUT, self.writer.envelope(&reply)).await??;
        Ok(())
    }

    async fn controller(&self, request: &Envelope, body: &Body) -> Result<Body, ProtocolFault> {
        let command = ControllerCommand::parse(&self.session, body)?;
        let controller = self
            .session
            .controller_id
            .as_deref()
            .expect("control handshake has controller");
        let id = request
            .request_id
            .as_deref()
            .expect("validated request has ID");
        self.workspaces
            .controller_command(controller, id, command)
            .await
    }

    async fn watch(
        mut self,
        reader: FrameReader<OwnedReadHalf>,
        mut live: LiveWatch,
    ) -> Result<()> {
        let mut incoming = Incoming::start(reader);
        let mut idle = Instant::now() + IDLE_TIMEOUT;
        loop {
            tokio::select! {
                _ = sleep_until(idle) => return Ok(()),
                request = incoming.requests.recv() => {
                    let Some(request) = request else { return Ok(()) };
                    if self.respond(request?).await? {
                        idle = Instant::now() + IDLE_TIMEOUT;
                    }
                }
                delta = live.watch.next() => {
                    let result = match delta {
                        Ok(delta) => snapshot::send_delta(
                            &self.session, &mut self.writer, &live.watch, &mut live.sequence, &delta,
                        ).await,
                        Err(fault) => Err(fault.into()),
                    };
                    if let Err(error) = result {
                        if let Some(fault) = error.downcast_ref::<ProtocolFault>() {
                            let frame = snapshot::event(
                                &self.session, &live.watch.id, None, Body::Error(fault.clone().into_wire()),
                            );
                            drop(live);
                            timeout(COMMAND_TIMEOUT, self.writer.envelope(&frame)).await??;
                        }
                        return Err(error);
                    }
                }
            }
        }
    }
}

fn status(metadata: &MetadataView, workspaces: &Workspaces) -> ConnectionStatus {
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
    let (terminal_live, collector) = workspaces.runtime_status();
    ConnectionStatus {
        terminal_live,
        persistence: persistence as i32,
        layout_recovery: recovery as i32,
        collector: Some(collector),
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
    let (input, output) = match role {
        PeerRole::Control | PeerRole::Presentation => {
            (FrameRole::StructuredOnly, FrameRole::StructuredOnly)
        }
        PeerRole::Tui => (FrameRole::StructuredOnly, FrameRole::TuiOutput),
        PeerRole::Collector => (FrameRole::Collector, FrameRole::Collector),
    };
    let mut writer = FrameWriter::start(write, output).await?;
    let mut reader = FrameReader::start(read, input).await?;
    let request = reader
        .next_envelope()
        .await?
        .context("missing control Hello")?;
    let profiles = match role {
        PeerRole::Control | PeerRole::Tui => qualified_profiles(),
        PeerRole::Collector | PeerRole::Presentation => &[],
    };
    match accept_hello_with_profiles(
        &request,
        role,
        &identity.host_id,
        &identity.epoch,
        capabilities,
        profiles,
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
