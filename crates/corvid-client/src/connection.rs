use anyhow::{Context, Result};
use corvid_protocol::{
    FrameError, FrameReader, FrameRole, FrameWriter, PeerRole, ProtocolFault, Session,
    check_hello_result, hello,
};
use tokio::io::{AsyncRead, AsyncWrite};
use uuid::Uuid;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod local;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use corvid_protocol::unix::SocketRole;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use local::{UnixConnection, open};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use tokio::net::unix::{OwnedReadHalf as LocalRead, OwnedWriteHalf as LocalWrite};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) type LocalConnection = UnixConnection;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use corvid_protocol::windows::DaemonPipe as SocketRole;
#[cfg(windows)]
pub(crate) use windows::{LocalConnection, LocalRead, LocalWrite, open};

pub(crate) struct Connection<R, W> {
    pub(crate) session: Session,
    pub(crate) reader: FrameReader<R>,
    pub(crate) writer: FrameWriter<W>,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Connection<R, W> {
    pub(crate) async fn negotiate(
        read: R,
        write: W,
        role: PeerRole,
        capabilities: Vec<String>,
        controller: Option<String>,
    ) -> Result<Self> {
        Self::negotiate_with_deadline(read, write, role, capabilities, controller, || {
            tokio::time::sleep(corvid_protocol::COMMAND_TIMEOUT)
        })
        .await
    }

    pub(crate) async fn negotiate_with_deadline<F: Future<Output = ()>>(
        read: R,
        write: W,
        role: PeerRole,
        capabilities: Vec<String>,
        controller: Option<String>,
        frame_deadline: impl FnOnce() -> F,
    ) -> Result<Self> {
        let (input, output) = match role {
            PeerRole::Control | PeerRole::Presentation | PeerRole::Upgrade => {
                (FrameRole::StructuredOnly, FrameRole::StructuredOnly)
            }
            PeerRole::Tui => (FrameRole::TuiOutput, FrameRole::StructuredOnly),
            PeerRole::Collector => (FrameRole::Collector, FrameRole::Collector),
        };
        let mut writer = FrameWriter::start(write, output).await?;
        let mut reader = read_preamble(read, input).await?;
        let greeting = hello(Uuid::new_v4().to_string(), controller, capabilities);
        writer.envelope(&greeting).await?;
        let reply = reader
            .next_envelope_with_deadline(frame_deadline)
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

pub(crate) async fn read_preamble<R: AsyncRead + Unpin>(
    read: R,
    role: FrameRole,
) -> Result<FrameReader<R>> {
    match FrameReader::start(read, role).await {
        Ok(reader) => Ok(reader),
        Err(FrameError::ProtocolMismatch) => Err(ProtocolFault::unsupported_framing().into()),
        Err(error) => Err(error.into()),
    }
}
