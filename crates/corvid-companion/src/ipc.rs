use std::future::Future;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_client::ControlEndpoint;
#[cfg(target_os = "linux")]
use corvid_protocol::unix::{BoundSocket, RuntimeDir, SocketRole};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{Envelope, GetPresentationSlots, Pong, PresentationSlots};
use corvid_protocol::{
    COMMAND_TIMEOUT, FrameReader, FrameRole, FrameWriter, PROTOCOL_MAJOR, PROTOCOL_MINOR, PeerRole,
    ProtocolFault, Session, accept_hello, check_hello_result, hello,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
#[cfg(target_os = "linux")]
use tokio::net::UnixStream as ClientStream;
#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeClient as ClientStream, NamedPipeServer};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use crate::Store;
#[cfg(windows)]
use crate::platform::{PipeListener, open_pipe};
use crate::service::{Call, Service};

pub const PRESENTATION_CAPABILITY: &str = "presentation_slots.v1";
pub const PRESENTATION_PAGE_SIZE: u32 = 32;
pub const PRESENTATION_ATTENTION_CAPABILITY: &str = "presentation_attention.v1";
pub const PRESENTATION_ATTENTION_LIMIT: usize = 512;
const LIST_HOSTS_CAPABILITY: &str = "list_hosts.v1";
#[cfg(windows)]
pub const WEZTERM_CAPABILITY: &str = "wezterm_local.v1";
#[cfg(windows)]
pub const WEZTERM_SELECTION_CAPABILITY: &str = "wezterm_selection.v1";
const CAPABILITIES: &[&str] = &[
    PRESENTATION_CAPABILITY,
    PRESENTATION_ATTENTION_CAPABILITY,
    LIST_HOSTS_CAPABILITY,
    #[cfg(windows)]
    WEZTERM_CAPABILITY,
    #[cfg(windows)]
    WEZTERM_SELECTION_CAPABILITY,
];
const ROLE: FrameRole = FrameRole::StructuredOnly;
const MAX_PEERS: usize = 8;
const MAX_CALLS: usize = 8;
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(not(windows))]
const CALL_TIMEOUT: Duration = Duration::from_secs(20);
// Windows focus also verifies the pane around the remote command and may
// wait briefly for the verified window to reach the foreground.
#[cfg(windows)]
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(target_os = "linux")]
pub type PresentationAddress = Path;
#[cfg(windows)]
pub type PresentationAddress = str;

pub async fn run(
    runtime: &PresentationAddress,
    state: &Path,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let store = Store::open(state)?;
    let endpoint = store.config().control_endpoint()?;
    run_service(runtime, store, endpoint, shutdown).await
}

/// Serve a Desktop plugin using the endpoint that Desktop verified.
pub async fn run_for_endpoint(
    runtime: &PresentationAddress,
    state: &Path,
    endpoint: ControlEndpoint,
    host: Uuid,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let store = Store::open(state)?;
    ensure!(
        store.config().host_id == host,
        "companion state belongs to a different host"
    );
    run_service(runtime, store, endpoint, shutdown).await
}

async fn run_service(
    runtime: &PresentationAddress,
    store: Store,
    endpoint: ControlEndpoint,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let host = store.config().host_id.to_string();
    let epoch = Uuid::new_v4().to_string();
    let mut service = Service::new(store, endpoint);

    #[cfg(target_os = "linux")]
    let (directory, instance);
    #[cfg(target_os = "linux")]
    let mut listener = {
        directory = RuntimeDir::create_all(runtime)?;
        instance = directory.lock_instance()?;
        instance.bind(SocketRole::Presentation).await?
    };
    #[cfg(windows)]
    let mut listener = PipeListener::bind(runtime)?;
    let (sender, calls) = mpsc::channel(MAX_CALLS);

    let result = tokio::select! {
        result = accept(&mut listener, sender, &host, &epoch) => result,
        result = service.run(calls) => result,
        _ = shutdown => Ok(()),
    };
    result.and(service.flush_status())
}

trait Listener {
    type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    fn next_peer(&mut self) -> impl Future<Output = Result<Option<Self::Stream>>> + Send;
}

#[cfg(target_os = "linux")]
impl Listener for BoundSocket<'_> {
    type Stream = ClientStream;

    async fn next_peer(&mut self) -> Result<Option<Self::Stream>> {
        self.accept().await
    }
}

#[cfg(windows)]
impl Listener for PipeListener {
    type Stream = NamedPipeServer;

    async fn next_peer(&mut self) -> Result<Option<Self::Stream>> {
        self.accept().await.map(Some)
    }
}

async fn accept(
    listener: &mut impl Listener,
    calls: mpsc::Sender<Call>,
    host: &str,
    epoch: &str,
) -> Result<()> {
    let mut peers = JoinSet::new();
    loop {
        tokio::select! {
            result = listener.next_peer() => match result {
                Ok(Some(stream)) if peers.len() < MAX_PEERS => {
                    peers.spawn(serve(stream, calls.clone(), host.to_owned(), epoch.to_owned()));
                }
                Ok(_) => {}
                Err(_) => sleep(Duration::from_millis(25)).await,
            },
            _ = peers.join_next(), if !peers.is_empty() => {}
        }
    }
}

type Reader<S> = FrameReader<ReadHalf<S>>;
type Writer<S> = FrameWriter<WriteHalf<S>>;

async fn open_frames<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
) -> Result<(Reader<S>, Writer<S>)> {
    let (input, output) = tokio::io::split(stream);
    let writer = FrameWriter::start(output, ROLE).await?;
    let reader = FrameReader::start(input, ROLE).await?;
    Ok((reader, writer))
}

async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    calls: mpsc::Sender<Call>,
    host: String,
    epoch: String,
) -> Result<()> {
    let mut peer = timeout(COMMAND_TIMEOUT, server_hello(stream, &host, &epoch)).await??;
    while let Some(request) = timeout(IDLE_TIMEOUT, peer.reader.next_envelope()).await?? {
        let outcome = match peer.session.validate_request(&request) {
            Ok(Body::Ping(_)) => Ok(Body::Pong(Pong {})),
            Ok(_) => dispatch(&calls, request.body.expect("validated body")).await,
            Err(fault) => Err(fault),
        };
        let body = outcome.unwrap_or_else(|fault| Body::Error(fault.into_wire()));
        let reply = peer.session.envelope(request.request_id, body);
        timeout(COMMAND_TIMEOUT, peer.writer.envelope(&reply)).await??;
    }
    Ok(())
}

async fn server_hello<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    host: &str,
    epoch: &str,
) -> Result<Peer<S>> {
    let (mut reader, mut writer) = open_frames(stream).await?;
    let request = reader
        .next_envelope()
        .await?
        .context("missing presentation Hello")?;
    let (session, reply) =
        match accept_hello(&request, PeerRole::Presentation, host, epoch, CAPABILITIES) {
            Ok(accepted) => accepted,
            Err(fault) => {
                let reply = Envelope {
                    protocol_major: PROTOCOL_MAJOR,
                    protocol_minor: PROTOCOL_MINOR,
                    request_id: request
                        .request_id
                        .filter(|id| id.len() <= 256 && !id.chars().any(char::is_control)),
                    body: Some(Body::Error(fault.clone().into_wire())),
                    ..Default::default()
                };
                writer.envelope(&reply).await?;
                return Err(fault.into());
            }
        };
    writer.envelope(&reply).await?;
    Ok(Peer {
        session,
        reader,
        writer,
    })
}

async fn dispatch(calls: &mpsc::Sender<Call>, body: Body) -> Result<Body, ProtocolFault> {
    let (reply, receive) = oneshot::channel();
    calls
        .try_send(Call { body, reply })
        .map_err(|_| ProtocolFault::new("LimitExceeded", "companion request queue unavailable"))?;
    timeout(CALL_TIMEOUT, receive)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_else(|| {
            Err(ProtocolFault::new(
                "OutcomeUnknown",
                "companion result was unavailable",
            ))
        })
}

struct Peer<S> {
    session: Session,
    reader: Reader<S>,
    writer: Writer<S>,
}

pub struct PresentationClient(Peer<ClientStream>);

impl PresentationClient {
    pub async fn connect(runtime: &PresentationAddress) -> Result<Self> {
        timeout(COMMAND_TIMEOUT, Self::negotiate(runtime))
            .await
            .context("companion Hello timed out")?
    }

    pub async fn connect_if_present(runtime: &PresentationAddress) -> Result<Option<Self>> {
        match Self::connect(runtime).await {
            Ok(client) => Ok(Some(client)),
            Err(error) if presentation_absent(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn negotiate(runtime: &PresentationAddress) -> Result<Self> {
        let stream = open_local(runtime).await.context("CompanionUnavailable")?;
        let (mut reader, mut writer) = open_frames(stream).await?;
        let request = hello(
            Uuid::new_v4().to_string(),
            None,
            vec![PRESENTATION_CAPABILITY.to_owned()],
        );
        writer.envelope(&request).await?;
        let reply = reader
            .next_envelope()
            .await?
            .context("companion closed during Hello")?;
        let session = check_hello_result(&request, &reply, PeerRole::Presentation)?;
        Ok(Self(Peer {
            session,
            reader,
            writer,
        }))
    }

    pub fn session(&self) -> &Session {
        &self.0.session
    }

    pub async fn slots(&mut self, config: Option<&crate::Config>) -> Result<PresentationSlots> {
        let reply = self
            .request(Body::GetPresentationSlots(GetPresentationSlots::default()))
            .await?;
        let Body::PresentationSlots(slots) = reply else {
            anyhow::bail!("unexpected slot response");
        };
        if let Some(config) = config {
            ensure!(
                self.session().host_id == config.host_id.to_string()
                    && slots.controller_id == config.controller_id.to_string(),
                "companion identity mismatch"
            );
        }
        Ok(slots)
    }

    /// A cancelled request leaves this connection unusable. Reconnect before requesting again.
    pub async fn request(&mut self, body: Body) -> Result<Body> {
        if matches!(&body, Body::ListHosts(_))
            && !self
                .session()
                .capabilities
                .iter()
                .any(|capability| capability == LIST_HOSTS_CAPABILITY)
        {
            return Err(ProtocolFault::new(
                "UnsupportedCapability",
                "companion does not support host listing",
            )
            .into());
        }
        timeout(CALL_TIMEOUT + COMMAND_TIMEOUT, self.0.exchange(body))
            .await
            .context("companion result timed out")?
    }
}

fn presentation_absent(error: &anyhow::Error) -> bool {
    use std::io::{Error, ErrorKind};

    #[cfg(target_os = "linux")]
    if error.downcast_ref::<nix::errno::Errno>() == Some(&nix::errno::Errno::ENOENT) {
        return true;
    }
    error.downcast_ref::<Error>().is_some_and(|error| {
        error.kind() == ErrorKind::NotFound
            || (cfg!(target_os = "linux") && error.kind() == ErrorKind::ConnectionRefused)
    })
}

#[cfg(target_os = "linux")]
async fn open_local(runtime: &PresentationAddress) -> Result<ClientStream> {
    RuntimeDir::open(runtime)?
        .connect(SocketRole::Presentation)
        .await
}

#[cfg(windows)]
async fn open_local(runtime: &PresentationAddress) -> Result<ClientStream> {
    open_pipe(runtime).await
}

impl<S: AsyncRead + AsyncWrite + Unpin> Peer<S> {
    async fn exchange(&mut self, body: Body) -> Result<Body> {
        let request = self
            .session
            .envelope(Some(Uuid::new_v4().to_string()), body);
        self.session.validate_request(&request)?;
        self.writer.envelope(&request).await?;
        let reply = self
            .reader
            .next_envelope()
            .await?
            .context("companion closed during command")?;
        self.session.validate_scope(&reply)?;
        ensure!(
            reply.request_id == request.request_id
                && reply.sequence.is_none()
                && reply.subscription_id.is_none(),
            "invalid companion response scope"
        );
        match reply.body.ok_or_else(|| {
            ProtocolFault::new("UnsupportedCapability", "unknown companion response body")
        })? {
            Body::Error(fault) => Err(ProtocolFault::from_wire(&fault).into()),
            body => Ok(body),
        }
    }
}
