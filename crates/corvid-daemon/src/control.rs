use std::time::Duration;

use anyhow::{Context, Result};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{Envelope, HostInfo, Pong};
use corvid_protocol::{
    COMMAND_TIMEOUT, FrameReader, FrameRole, FrameWriter, PROTOCOL_MAJOR, PROTOCOL_MINOR, PeerRole,
    ProtocolFault, Session, accept_hello,
};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::timeout;

use crate::identity::Identity;

const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const CAPABILITIES: &[&str] = &["host.v1", "ping.v1"];

pub(crate) async fn serve(stream: UnixStream, identity: &Identity) -> Result<()> {
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
