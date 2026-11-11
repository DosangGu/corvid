use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
#[cfg(target_os = "linux")]
use corvid_protocol::unix::RuntimeDir;
use corvid_protocol::upgrade::decode_state_path;
use corvid_protocol::wire::UpgradeDaemon;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::{
    COMMAND_TIMEOUT, DAEMON_HANDOFF_CAPABILITY, DAEMON_UPGRADE_CAPABILITY, PeerRole, ProtocolFault,
};
use tokio::time::timeout;
use uuid::Uuid;

use crate::connection::{LocalConnection, SocketRole, open};

pub struct UpgradeTarget {
    pub state_directory: PathBuf,
    pub host_id: String,
    pub server_epoch: String,
}

#[cfg(target_os = "linux")]
pub async fn prepare_upgrade(runtime: &RuntimeDir) -> Result<UpgradeTarget> {
    prepare_upgrade_for(runtime, None).await
}

#[cfg(target_os = "linux")]
pub async fn prepare_upgrade_for(
    runtime: &RuntimeDir,
    expected_host: Option<Uuid>,
) -> Result<UpgradeTarget> {
    prepare_daemon_upgrade(runtime.path(), expected_host).await
}

pub async fn prepare_daemon_upgrade(
    runtime: &Path,
    expected_host: Option<Uuid>,
) -> Result<UpgradeTarget> {
    let stream = match open(runtime, SocketRole::Upgrade).await {
        Ok(stream) => stream,
        Err(error)
            if error.downcast_ref::<io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::ConnectionRefused
                )
            }) =>
        {
            bail!(ProtocolFault::new(
                "UnsupportedCapability",
                "this daemon requires a manual restart. Close live terminals, then restart it with its service manager or the updated binary"
            ));
        }
        Err(error) => return Err(error),
    };
    let mut connection = LocalConnection::establish(
        stream,
        PeerRole::Upgrade,
        vec![DAEMON_UPGRADE_CAPABILITY.into()],
    )
    .await?;
    ensure!(
        expected_host.is_none_or(|host| connection.session.host_id == host.to_string()),
        ProtocolFault::new(
            "HostMismatch",
            "SSH alias points to a different Corvid host"
        )
    );
    let target = timeout(COMMAND_TIMEOUT, async {
        let request = connection.session.envelope(
            Some(Uuid::new_v4().to_string()),
            Body::UpgradeDaemon(UpgradeDaemon {
                preserve_terminals: connection
                    .session
                    .capabilities
                    .iter()
                    .any(|capability| capability == DAEMON_HANDOFF_CAPABILITY),
            }),
        );
        connection.writer.envelope(&request).await?;
        let reply = connection
            .reader
            .next_envelope()
            .await?
            .context("daemon closed during upgrade")?;
        connection.session.validate_scope(&reply)?;
        ensure!(
            reply.request_id == request.request_id,
            "upgrade request ID mismatch"
        );
        match reply.body {
            Some(Body::DaemonUpgradeReady(ready)) => Ok(ready.state_directory),
            Some(Body::Error(fault)) => Err(ProtocolFault::from_wire(&fault).into()),
            _ => bail!("unexpected daemon upgrade response"),
        }
    })
    .await
    .context("daemon upgrade request timed out")??;
    let state_directory = decode_state_path(target).context("invalid daemon state directory")?;
    Ok(UpgradeTarget {
        state_directory,
        host_id: connection.session.host_id,
        server_epoch: connection.session.server_epoch,
    })
}
