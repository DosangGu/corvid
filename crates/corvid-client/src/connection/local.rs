use std::path::Path;

use anyhow::{Context, Result};
use corvid_protocol::unix::{RuntimeDir, SocketRole};
use corvid_protocol::{COMMAND_TIMEOUT, PeerRole};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::timeout;

use super::Connection;

pub(crate) type UnixConnection = Connection<OwnedReadHalf, OwnedWriteHalf>;

pub(crate) async fn open(path: &Path, role: SocketRole) -> Result<UnixStream> {
    let runtime = RuntimeDir::open(path).context("DaemonUnavailable")?;
    runtime.connect(role).await.context("DaemonUnavailable")
}

impl UnixConnection {
    pub(crate) async fn establish(
        stream: UnixStream,
        role: PeerRole,
        capabilities: Vec<String>,
    ) -> Result<Self> {
        let (read, write) = stream.into_split();
        timeout(
            COMMAND_TIMEOUT,
            Self::negotiate(read, write, role, capabilities, None),
        )
        .await
        .context("daemon handshake timed out")?
    }
}
