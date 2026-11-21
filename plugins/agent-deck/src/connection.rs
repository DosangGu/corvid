use anyhow::{Context, Result, ensure};
use corvid_async::time::timeout;
use corvid_async::{mpsc, oneshot};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{Envelope, OpenAgentDeck};
use corvid_protocol::{
    COMMAND_TIMEOUT, FrameReader, FrameRole, FrameWriter, PeerRole, Session, check_hello_result,
    hello,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use uuid::Uuid;

pub const CAPABILITY: &str = "agent_deck.v1";
pub const LIFECYCLE_CAPABILITY: &str = "agent_deck.lifecycle.v1";
pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}
pub type Stream = Box<dyn Duplex>;
pub type Reader = FrameReader<ReadHalf<Stream>>;
pub type Writer = FrameWriter<WriteHalf<Stream>>;

pub struct Peer {
    pub stream: Stream,
    pub pid: u32,
}

pub struct Client {
    pub session: Session,
    pub reader: Reader,
    pub writer: Writer,
    host: Uuid,
    controller: Uuid,
}

pub async fn frames(stream: Stream) -> Result<(Reader, Writer)> {
    let (read, write) = tokio::io::split(stream);
    let writer = FrameWriter::start(write, FrameRole::StructuredOnly).await?;
    let reader = FrameReader::start(read, FrameRole::StructuredOnly).await?;
    Ok((reader, writer))
}

impl Client {
    pub async fn connect(host: Uuid, controller: Uuid, desktop: bool) -> Result<Self> {
        let stream = timeout(COMMAND_TIMEOUT, connect(controller)).await??;
        let mut client = Self::handshake(stream, host, controller).await?;
        client.activate(desktop).await?;
        Ok(client)
    }

    pub(crate) async fn handshake(stream: Stream, host: Uuid, controller: Uuid) -> Result<Self> {
        timeout(COMMAND_TIMEOUT, async {
            let (mut reader, mut writer) = frames(stream).await?;
            let request = hello(Uuid::new_v4().to_string(), None, vec![CAPABILITY.into()]);
            writer.envelope(&request).await?;
            let reply = reader
                .next_envelope()
                .await?
                .context("Agent Deck closed during Hello")?;
            let session = check_hello_result(&request, &reply, PeerRole::Presentation)?;
            ensure!(
                session.host_id == host.to_string(),
                "Agent Deck belongs to a different host"
            );
            Ok(Self {
                session,
                reader,
                writer,
                host,
                controller,
            })
        })
        .await
        .context("Agent Deck connection timed out")?
    }

    pub async fn activate(&mut self, desktop: bool) -> Result<()> {
        timeout(COMMAND_TIMEOUT, async {
            let id = self.open(desktop).await?;
            let reply = self
                .reader
                .next_envelope()
                .await?
                .context("Agent Deck did not open")?;
            self.session.validate_scope(&reply)?;
            ensure!(
                reply.request_id.as_deref() == Some(id.as_str())
                    && matches!(reply.body, Some(Body::AgentDeckOpened(_))),
                "Agent Deck registration rejected"
            );
            Ok(())
        })
        .await
        .context("Agent Deck connection timed out")?
    }

    #[cfg(not(windows))]
    pub(crate) async fn retire(&mut self) -> Result<()> {
        let id = Uuid::new_v4().to_string();
        timeout(COMMAND_TIMEOUT, async {
            self.writer
                .envelope(&self.session.envelope(
                    Some(id.clone()),
                    Body::RetireAgentDeck(corvid_protocol::wire::RetireAgentDeck {
                        controller_id: self.controller.to_string(),
                    }),
                ))
                .await?;
            let reply = self
                .reader
                .next_envelope()
                .await?
                .context("missing Agent Deck retirement response")?;
            self.session.validate_scope(&reply)?;
            ensure!(
                reply.request_id.as_deref() == Some(id.as_str())
                    && matches!(reply.body, Some(Body::AgentDeckRetired(_))),
                "Agent Deck refused to retire"
            );
            Ok(())
        })
        .await
        .context("retiring Agent Deck timed out")?
    }

    pub async fn open(&mut self, desktop: bool) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        self.writer
            .envelope(&self.session.envelope(
                Some(id.clone()),
                Body::OpenAgentDeck(OpenAgentDeck {
                    host_id: self.host.to_string(),
                    controller_id: self.controller.to_string(),
                    register_desktop: desktop,
                }),
            ))
            .await?;
        Ok(id)
    }

    pub async fn reply(&mut self, id: String, body: Body) -> Result<()> {
        timeout(
            COMMAND_TIMEOUT,
            self.writer.envelope(&self.session.envelope(Some(id), body)),
        )
        .await??;
        Ok(())
    }
}

#[cfg(windows)]
async fn connect(controller: Uuid) -> Result<Stream> {
    Ok(Box::new(
        corvid_protocol::windows::open_scoped(
            corvid_protocol::paths::AGENT_DECK_PIPE_PREFIX,
            &corvid_protocol::paths::controller_pipe_instance(controller),
        )
        .await?,
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn connect(controller: Uuid) -> Result<Stream> {
    use corvid_protocol::unix::{RuntimeDir, SocketRole};
    Ok(Box::new(
        RuntimeDir::open(&corvid_protocol::paths::agent_deck_socket(controller)?.prepare()?)?
            .connect(SocketRole::Presentation)
            .await?,
    ))
}

pub async fn listen(
    controller: Uuid,
    peers: mpsc::Sender<Peer>,
    ready: oneshot::Sender<()>,
) -> Result<()> {
    #[cfg(windows)]
    let mut listener = corvid_protocol::windows::PipeListener::bind_scoped(
        corvid_protocol::paths::AGENT_DECK_PIPE_PREFIX,
        &corvid_protocol::paths::controller_pipe_instance(controller),
    )?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let directory = corvid_protocol::unix::RuntimeDir::create_all(
        &corvid_protocol::paths::agent_deck_socket(controller)?.prepare()?,
    )?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let instance = directory.lock_instance()?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let listener = instance
        .bind(corvid_protocol::unix::SocketRole::Presentation)
        .await?;
    let _ = ready.send(());
    loop {
        #[cfg(windows)]
        let stream = listener.accept().await?;
        #[cfg(windows)]
        let pid = {
            use std::os::windows::io::AsRawHandle;
            let mut pid = 0;
            ensure!(
                unsafe {
                    windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId(
                        stream.as_raw_handle(),
                        &mut pid,
                    )
                } != 0,
                "cannot identify Desktop peer"
            );
            pid
        };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let Some(stream) = listener.accept().await? else {
            continue;
        };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let pid = stream
            .peer_cred()?
            .pid()
            .and_then(|pid| u32::try_from(pid).ok())
            .unwrap_or(0);
        let _ = peers.try_send(Peer {
            stream: Box::new(stream),
            pid,
        });
    }
}

pub fn request_id(request: &Envelope) -> Result<Uuid> {
    ensure!(
        request.sequence.is_none() && request.subscription_id.is_none(),
        "plugin command contains event metadata"
    );
    let id = Uuid::parse_str(
        request
            .request_id
            .as_deref()
            .context("missing plugin request ID")?,
    )?;
    ensure!(!id.is_nil(), "invalid plugin request ID");
    Ok(id)
}
