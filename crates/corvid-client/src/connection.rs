use anyhow::{Context, Result};
use corvid_protocol::{
    FrameError, FrameReader, FrameRole, FrameWriter, PeerRole, ProtocolFault, Session,
    check_hello_result, hello,
};
use tokio::io::{AsyncRead, AsyncWrite};
use uuid::Uuid;

#[cfg(target_os = "linux")]
mod local;
#[cfg(target_os = "linux")]
pub(crate) use local::{UnixConnection, open};

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
        let (input, output) = match role {
            PeerRole::Control | PeerRole::Presentation => {
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
