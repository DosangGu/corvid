use std::ffi::OsString;
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use corvid_protocol::unix::{RuntimeDir, SocketRole};
use corvid_protocol::wire::UpgradeDaemon;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::{COMMAND_TIMEOUT, DAEMON_UPGRADE_CAPABILITY, PeerRole, ProtocolFault};
use tokio::time::timeout;
use uuid::Uuid;

use crate::connection::UnixConnection;

pub struct UpgradeTarget {
    pub state_directory: PathBuf,
    pub host_id: String,
    pub server_epoch: String,
}

pub async fn prepare_upgrade(runtime: &RuntimeDir) -> Result<UpgradeTarget> {
    let stream = match runtime.connect(SocketRole::Upgrade).await {
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
    let mut connection = UnixConnection::establish(
        stream,
        PeerRole::Upgrade,
        vec![DAEMON_UPGRADE_CAPABILITY.into()],
    )
    .await?;
    let target = timeout(COMMAND_TIMEOUT, async {
        let request = connection.session.envelope(
            Some(Uuid::new_v4().to_string()),
            Body::UpgradeDaemon(UpgradeDaemon {}),
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
    ensure!(!target.contains(&0), "invalid daemon state directory");
    let state_directory = PathBuf::from(OsString::from_vec(target));
    ensure!(
        state_directory.is_absolute(),
        "invalid daemon state directory"
    );
    Ok(UpgradeTarget {
        state_directory,
        host_id: connection.session.host_id,
        server_epoch: connection.session.server_epoch,
    })
}
