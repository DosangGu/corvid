use std::path::Path;

use anyhow::{Context, Result};
use corvid_protocol::unix::{RuntimeDir, SocketRole};
use corvid_protocol::{
    COMMAND_TIMEOUT, FrameReader, FrameRole, FrameWriter, PeerRole, Session, check_hello_result,
    hello,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::timeout;
use uuid::Uuid;

pub(crate) struct Connection<R = OwnedReadHalf, W = OwnedWriteHalf> {
    pub(crate) session: Session,
    pub(crate) reader: FrameReader<R>,
    pub(crate) writer: FrameWriter<W>,
}

pub(crate) async fn open(path: &Path, role: SocketRole) -> Result<UnixStream> {
    let runtime = RuntimeDir::open(path).context("DaemonUnavailable")?;
    runtime.connect(role).await.context("DaemonUnavailable")
}

impl Connection {
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

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Connection<R, W> {
    pub(crate) async fn negotiate(
        read: R,
        write: W,
        role: PeerRole,
        capabilities: Vec<String>,
        controller: Option<String>,
    ) -> Result<Self> {
        let (input, output) = match role {
            PeerRole::Control => (FrameRole::StructuredOnly, FrameRole::StructuredOnly),
            PeerRole::Tui => (FrameRole::TuiOutput, FrameRole::StructuredOnly),
            PeerRole::Collector => (FrameRole::Collector, FrameRole::Collector),
        };
        let mut writer = FrameWriter::start(write, output).await?;
        let mut reader = FrameReader::start(read, input).await?;
        let greeting = hello(Uuid::new_v4().to_string(), controller, capabilities);
        writer.envelope(&greeting).await?;
        let reply = reader
            .next_envelope()
            .await?
            .context("daemon closed during Hello")?;
        let session = check_hello_result(&greeting, &reply, role)?;
        Ok(Self {
            session,
            reader,
            writer,
        })
    }
}
