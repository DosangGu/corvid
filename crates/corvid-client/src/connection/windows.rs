use std::path::Path;

use anyhow::{Context, Result};
use corvid_protocol::{COMMAND_TIMEOUT, PeerRole};
use tokio::io::{ReadHalf, WriteHalf, split};
use tokio::net::windows::named_pipe::NamedPipeClient;
use tokio::time::timeout;

use super::{Connection, SocketRole};

pub(crate) type LocalRead = ReadHalf<NamedPipeClient>;
pub(crate) type LocalWrite = WriteHalf<NamedPipeClient>;
pub(crate) type LocalConnection = Connection<LocalRead, LocalWrite>;

pub(crate) async fn open(path: &Path, role: SocketRole) -> Result<NamedPipeClient> {
    role.connect(path).await.context("DaemonUnavailable")
}

impl LocalConnection {
    pub(crate) async fn establish(
        stream: NamedPipeClient,
        role: PeerRole,
        capabilities: Vec<String>,
    ) -> Result<Self> {
        let (read, write) = split(stream);
        timeout(
            COMMAND_TIMEOUT,
            Self::negotiate(read, write, role, capabilities, None),
        )
        .await
        .context("daemon handshake timed out")?
    }
}
