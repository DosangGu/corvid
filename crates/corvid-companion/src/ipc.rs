use std::future::Future;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_protocol::unix::{BoundSocket, RuntimeDir, SocketRole};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{Envelope, Pong};
use corvid_protocol::{
    COMMAND_TIMEOUT, FrameReader, FrameRole, FrameWriter, PROTOCOL_MAJOR, PROTOCOL_MINOR, PeerRole,
    ProtocolFault, Session, accept_hello, check_hello_result, hello,
};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use crate::Store;
use crate::service::{Call, Service};

pub const PRESENTATION_CAPABILITY: &str = "presentation_slots.v1";
const ROLE: FrameRole = FrameRole::StructuredOnly;
const MAX_PEERS: usize = 8;
const MAX_CALLS: usize = 8;
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(20);

pub async fn run(runtime: &Path, state: &Path, shutdown: impl Future<Output = ()>) -> Result<()> {
    let store = Store::open(state)?;
    let host = store.config().host_id.to_string();
    let epoch = Uuid::new_v4().to_string();
    let service = Service::new(store)?;

    let directory = RuntimeDir::create_all(runtime)?;
    let instance = directory.lock_instance()?;
    let socket = instance.bind(SocketRole::Presentation).await?;
    let (sender, calls) = mpsc::channel(MAX_CALLS);

    tokio::select! {
        result = accept(&socket, sender, &host, &epoch) => result,
        result = service.run(calls) => result,
        _ = shutdown => Ok(()),
    }
}

async fn accept(
    socket: &BoundSocket<'_>,
    calls: mpsc::Sender<Call>,
    host: &str,
    epoch: &str,
) -> Result<()> {
    let mut peers = JoinSet::new();
    loop {
        tokio::select! {
            result = socket.accept() => {
                let Ok(stream) = result else {
                    sleep(Duration::from_millis(25)).await;
                    continue;
                };
                if peers.len() < MAX_PEERS {
                    peers.spawn(serve(stream, calls.clone(), host.to_owned(), epoch.to_owned()));
                }
            }
            _ = peers.join_next(), if !peers.is_empty() => {}
        }
    }
}

type Reader = FrameReader<OwnedReadHalf>;
type Writer = FrameWriter<OwnedWriteHalf>;

async fn open_frames(stream: UnixStream) -> Result<(Reader, Writer)> {
    let (input, output) = stream.into_split();
    let writer = FrameWriter::start(output, ROLE).await?;
    let reader = FrameReader::start(input, ROLE).await?;
    Ok((reader, writer))
}

async fn serve(
    stream: UnixStream,
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

async fn server_hello(stream: UnixStream, host: &str, epoch: &str) -> Result<PresentationClient> {
    let (mut reader, mut writer) = open_frames(stream).await?;
    let request = reader
        .next_envelope()
        .await?
        .context("missing presentation Hello")?;
    let (session, reply) = match accept_hello(
        &request,
        PeerRole::Presentation,
        host,
        epoch,
        &[PRESENTATION_CAPABILITY],
    ) {
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
    Ok(PresentationClient {
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

pub struct PresentationClient {
    session: Session,
    reader: Reader,
    writer: Writer,
}

impl PresentationClient {
    pub async fn connect(runtime: &Path) -> Result<Self> {
        timeout(COMMAND_TIMEOUT, Self::negotiate(runtime))
            .await
            .context("companion Hello timed out")?
    }

    async fn negotiate(runtime: &Path) -> Result<Self> {
        let directory = RuntimeDir::open(runtime).context("CompanionUnavailable")?;
        let stream = directory
            .connect(SocketRole::Presentation)
            .await
            .context("CompanionUnavailable")?;
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
        Ok(Self {
            session,
            reader,
            writer,
        })
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// A cancelled request leaves this connection unusable. Reconnect to refresh slots.
    pub async fn request(&mut self, body: Body) -> Result<Body> {
        timeout(CALL_TIMEOUT + COMMAND_TIMEOUT, self.exchange(body))
            .await
            .context("companion result timed out")?
    }

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
        match reply.body.context("unknown companion response body")? {
            Body::Error(fault) => Err(ProtocolFault::from_wire(&fault).into()),
            body => Ok(body),
        }
    }
}
