use std::path::Path;

use anyhow::{Context, Result, bail};
use compio::io::{AsyncRead, AsyncWrite, util::Splittable};
use compio::time::timeout;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{GetHost, HostInfo, Ping};
use corvid_protocol::{
    COMMAND_TIMEOUT, CompioFrameReader, CompioFrameWriter, FrameError, FrameRole, PeerRole,
    ProtocolFault, Session, check_hello_result, hello,
};
use uuid::Uuid;

use crate::connection::SocketRole;
use crate::control::check_reply;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) type LocalStream = compio::net::UnixStream;
#[cfg(windows)]
pub(crate) type LocalStream = compio::fs::named_pipe::NamedPipeClient;

pub(crate) type LocalConnection = Connection<LocalStream, LocalStream>;

pub(crate) struct Connection<R, W> {
    pub(crate) reader: CompioFrameReader<R>,
    pub(crate) writer: CompioFrameWriter<W>,
    pub(crate) session: Session,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) async fn open(runtime: &Path, role: SocketRole) -> Result<LocalStream> {
    corvid_protocol::unix::RuntimeDir::open(runtime)?
        .connect_compio(role)
        .await
}

#[cfg(windows)]
pub(crate) async fn open(runtime: &Path, role: SocketRole) -> Result<LocalStream> {
    role.connect_compio(runtime).await
}

impl LocalConnection {
    pub(crate) async fn establish(
        stream: LocalStream,
        peer: PeerRole,
        capabilities: Vec<String>,
        controller: Option<String>,
    ) -> Result<Self> {
        let (read, write) = stream.split();
        Self::negotiate(read, write, peer, capabilities, controller).await
    }
}

impl<R: AsyncRead, W: AsyncWrite> Connection<R, W> {
    pub(crate) async fn negotiate(
        read: R,
        write: W,
        peer: PeerRole,
        capabilities: Vec<String>,
        controller: Option<String>,
    ) -> Result<Self> {
        let (input, output) = match peer {
            PeerRole::Collector => (FrameRole::Collector, FrameRole::Collector),
            PeerRole::Tui => (FrameRole::TuiOutput, FrameRole::StructuredOnly),
            PeerRole::Control | PeerRole::Presentation | PeerRole::Upgrade => {
                (FrameRole::StructuredOnly, FrameRole::StructuredOnly)
            }
        };
        let mut writer = CompioFrameWriter::start(write, output).await?;
        let mut reader = match CompioFrameReader::start(read, input).await {
            Ok(reader) => reader,
            Err(FrameError::ProtocolMismatch) => {
                return Err(ProtocolFault::unsupported_framing().into());
            }
            Err(error) => return Err(error.into()),
        };
        let greeting = hello(Uuid::new_v4().to_string(), controller, capabilities);
        writer.envelope(&greeting).await?;
        let reply = reader
            .next_envelope()
            .await?
            .context("daemon closed during Hello")?;
        let session = check_hello_result(&greeting, &reply, peer)?;
        Ok(Self {
            reader,
            writer,
            session,
        })
    }
}

/// Probe a running Corvid daemon. Run this future on a Compio runtime.
pub async fn compio_ping(path: &Path) -> Result<()> {
    match control(path, "ping.v1", Body::Ping(Ping {})).await? {
        Body::Pong(_) => Ok(()),
        _ => bail!("unexpected response to Ping"),
    }
}

/// Query a running Corvid daemon's host identity on a Compio runtime.
pub async fn compio_host(path: &Path) -> Result<HostInfo> {
    match control(path, "host.v1", Body::GetHost(GetHost {})).await? {
        Body::HostInfo(host) => Ok(host),
        _ => bail!("unexpected response to GetHost"),
    }
}

async fn control(path: &Path, capability: &str, body: Body) -> Result<Body> {
    let stream = open(path, SocketRole::Control).await?;
    let mut connection = timeout(
        COMMAND_TIMEOUT,
        LocalConnection::establish(
            stream,
            PeerRole::Control,
            vec![capability.to_owned()],
            Some(Uuid::new_v4().to_string()),
        ),
    )
    .await
    .context("daemon handshake timed out")??;
    timeout(COMMAND_TIMEOUT, async {
        let session = &connection.session;
        let request = session.envelope(Some(Uuid::new_v4().to_string()), body);
        connection.writer.envelope(&request).await?;
        let reply = connection
            .reader
            .next_envelope()
            .await?
            .context("daemon closed during command")?;
        check_reply(session, &request, reply)
    })
    .await
    .context("control command timed out")?
}
