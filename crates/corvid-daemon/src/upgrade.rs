use std::future::pending;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
#[cfg(target_os = "linux")]
use corvid_protocol::unix::BoundSocket;
use corvid_protocol::upgrade::encode_state_path;
#[cfg(windows)]
use corvid_protocol::windows::PipeListener;
use corvid_protocol::wire::DaemonUpgradeReady;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::{
    COMMAND_TIMEOUT, DAEMON_HANDOFF_CAPABILITY, DAEMON_UPGRADE_CAPABILITY, FrameWriter, PeerRole,
    ProtocolFault, Session,
};
use tokio::time::{sleep, timeout};

use crate::control::handshake;
use crate::identity::Identity;
use crate::platform::{Stream, WriteHalf};

pub(crate) struct Request {
    session: Session,
    writer: FrameWriter<WriteHalf>,
    id: Option<String>,
    pub(crate) preserve_terminals: bool,
}

#[cfg(target_os = "linux")]
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

#[cfg(windows)]
pub(crate) async fn listen(
    socket: Option<&tokio::sync::Mutex<PipeListener>>,
    identity: &Identity,
) -> Request {
    let Some(socket) = socket else {
        return pending().await;
    };
    loop {
        match socket.lock().await.accept().await {
            Ok(stream) => {
                if let Ok(Ok(Some(request))) =
                    timeout(COMMAND_TIMEOUT, read(stream, identity)).await
                {
                    return request;
                }
            }
            Err(_) => sleep(Duration::from_millis(25)).await,
        }
    }
}

async fn read(stream: Stream, identity: &Identity) -> Result<Option<Request>> {
    let Some((session, mut reader, mut writer)) = handshake(
        stream,
        identity,
        PeerRole::Upgrade,
        &[DAEMON_UPGRADE_CAPABILITY, DAEMON_HANDOFF_CAPABILITY],
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
    let preserve_terminals =
        matches!(&request.body, Some(Body::UpgradeDaemon(upgrade)) if upgrade.preserve_terminals);
    Ok(Some(Request {
        session,
        writer,
        id: request.request_id,
        preserve_terminals,
    }))
}

impl Request {
    pub(crate) async fn respond(
        mut self,
        state_path: &Path,
        prepare: impl std::future::Future<Output = Result<()>>,
    ) -> Result<bool> {
        let body = match prepare.await {
            Ok(()) => Body::DaemonUpgradeReady(DaemonUpgradeReady {
                state_directory: encode_state_path(state_path),
            }),
            Err(error) => {
                let fault = error
                    .downcast_ref::<ProtocolFault>()
                    .cloned()
                    .unwrap_or_else(|| {
                        ProtocolFault::new(
                            "HandoffUnavailable",
                            "terminal handoff could not be prepared",
                        )
                    });
                Body::Error(fault.into_wire())
            }
        };
        let approved = matches!(body, Body::DaemonUpgradeReady(_));
        let reply = self.session.envelope(self.id, body);
        timeout(COMMAND_TIMEOUT, self.writer.envelope(&reply)).await??;
        Ok(approved)
    }
}
