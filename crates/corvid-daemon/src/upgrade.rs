use std::future::pending;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use corvid_protocol::unix::BoundSocket;
use corvid_protocol::wire::DaemonUpgradeReady;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::{
    COMMAND_TIMEOUT, DAEMON_UPGRADE_CAPABILITY, FrameWriter, PeerRole, ProtocolFault, Session,
};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::time::{sleep, timeout};

use crate::control::handshake;
use crate::identity::Identity;
use crate::metadata::MetadataView;

pub(crate) struct Request {
    session: Session,
    writer: FrameWriter<OwnedWriteHalf>,
    id: Option<String>,
}

pub(crate) async fn listen(socket: Option<&BoundSocket<'_>>, identity: &Identity) -> Request {
    let Some(socket) = socket else {
        return pending().await;
    };
    loop {
        match socket.accept().await {
            Ok(Some(stream)) => {
                if let Ok(Ok(Some(request))) =
                    timeout(COMMAND_TIMEOUT, read(stream, identity)).await
                {
                    return request;
                }
            }
            Ok(None) => {}
            Err(_) => sleep(Duration::from_millis(25)).await,
        }
    }
}

async fn read(stream: UnixStream, identity: &Identity) -> Result<Option<Request>> {
    let Some((session, mut reader, mut writer)) = handshake(
        stream,
        identity,
        PeerRole::Upgrade,
        &[DAEMON_UPGRADE_CAPABILITY],
    )
    .await?
    else {
        return Ok(None);
    };
    let request = reader
        .next_envelope()
        .await?
        .context("missing daemon upgrade request")?;
    if let Err(fault) = session.validate_request(&request) {
        let reply = session.envelope(request.request_id, Body::Error(fault.into_wire()));
        writer.envelope(&reply).await?;
        return Ok(None);
    }
    Ok(Some(Request {
        session,
        writer,
        id: request.request_id,
    }))
}

impl Request {
    pub(crate) async fn respond(
        mut self,
        idle: bool,
        mut metadata: MetadataView,
        state_path: &Path,
    ) -> Result<bool> {
        let body = if !idle {
            let mut fault = ProtocolFault::new(
                "DaemonBusy",
                "exit all live terminals and retry the upgrade",
            );
            fault.retryable = true;
            Body::Error(fault.into_wire())
        } else if !timeout(COMMAND_TIMEOUT, metadata.wait_saved())
            .await
            .unwrap_or(false)
        {
            let fault = ProtocolFault::new(
                "PersistenceUnavailable",
                "daemon metadata could not be saved before upgrade",
            );
            Body::Error(fault.into_wire())
        } else {
            Body::DaemonUpgradeReady(DaemonUpgradeReady {
                state_directory: state_path.as_os_str().as_encoded_bytes().to_vec(),
            })
        };
        let approved = matches!(body, Body::DaemonUpgradeReady(_));
        let reply = self.session.envelope(self.id, body);
        timeout(COMMAND_TIMEOUT, self.writer.envelope(&reply)).await??;
        Ok(approved)
    }
}
