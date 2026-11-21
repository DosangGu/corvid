use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use compio::io::{AsyncRead, AsyncWrite};
use compio::runtime::{Runtime, spawn};
use corvid_protocol::wire::Envelope;
use corvid_protocol::{COMMAND_TIMEOUT, Frame, FrameError, PeerRole, ProtocolFault, Session};
use tokio::sync::{mpsc, oneshot};

use crate::compio_connection::{self, Connection, LocalConnection};
use crate::connection::SocketRole;

pub(super) type ReadQueue = mpsc::Receiver<Result<Frame, FrameError>>;
pub(crate) struct WriteQueue(mpsc::Sender<WriteRequest>);

struct WriteRequest {
    message: Envelope,
    complete: oneshot::Sender<Result<(), FrameError>>,
}

enum Target {
    Local(PathBuf),
    Relay(Command),
}

struct ConnectRequest {
    target: Target,
    peer: PeerRole,
    capabilities: Vec<String>,
    controller: Option<String>,
    connected: oneshot::Sender<Result<(Session, ReadQueue, WriteQueue)>>,
}

struct SetupRequest {
    command: Command,
    payload: Option<(Vec<u8>, Vec<u8>)>,
    limit: usize,
    timeout: Duration,
    timeout_context: &'static str,
    output: oneshot::Sender<Result<super::process::Output>>,
}

enum Request {
    Connect(ConnectRequest),
    Setup(SetupRequest),
}

static DRIVER: OnceLock<io::Result<mpsc::Sender<Request>>> = OnceLock::new();

pub(super) async fn connect(
    path: &Path,
    peer: PeerRole,
    capabilities: Vec<String>,
    controller: Option<String>,
) -> Result<(Session, ReadQueue, WriteQueue)> {
    open(
        Target::Local(path.to_owned()),
        peer,
        capabilities,
        controller,
    )
    .await
}

pub(super) async fn relay(
    command: Command,
    peer: PeerRole,
    capabilities: Vec<String>,
    controller: Option<String>,
) -> Result<(Session, ReadQueue, WriteQueue)> {
    open(Target::Relay(command), peer, capabilities, controller).await
}

async fn open(
    target: Target,
    peer: PeerRole,
    capabilities: Vec<String>,
    controller: Option<String>,
) -> Result<(Session, ReadQueue, WriteQueue)> {
    let (connected, ready) = oneshot::channel();
    dispatch(Request::Connect(ConnectRequest {
        target,
        peer,
        capabilities,
        controller,
        connected,
    }))
    .await?;
    ready.await.context("Compio IPC driver stopped")?
}

pub(super) async fn command_output(
    command: Command,
    payload: Option<(&[u8], &[u8])>,
    limit: usize,
    timeout: Duration,
    timeout_context: &'static str,
) -> Result<super::process::Output> {
    let (output, ready) = oneshot::channel();
    dispatch(Request::Setup(SetupRequest {
        command,
        payload: payload.map(|(header, binary)| (header.to_vec(), binary.to_vec())),
        limit,
        timeout,
        timeout_context,
        output,
    }))
    .await?;
    ready.await.context("Compio IPC driver stopped")?
}

async fn dispatch(request: Request) -> Result<()> {
    let driver = DRIVER
        .get_or_init(start_driver)
        .as_ref()
        .map_err(|error| io::Error::new(error.kind(), error.to_string()))
        .context("starting Compio IPC thread")?;
    driver
        .send(request)
        .await
        .context("Compio IPC driver is unavailable")
}

fn start_driver() -> io::Result<mpsc::Sender<Request>> {
    let (send, mut requests) = mpsc::channel::<Request>(32);
    thread::Builder::new()
        .name("corvid-ipc".into())
        .spawn(move || match Runtime::new() {
            Ok(runtime) => runtime.block_on(async {
                while let Some(request) = requests.recv().await {
                    match request {
                        Request::Connect(request) => spawn(serve(request)).detach(),
                        Request::Setup(request) => spawn(set_up(request)).detach(),
                    }
                }
            }),
            Err(error) => {
                while let Some(request) = requests.blocking_recv() {
                    let report =
                        anyhow::Error::new(io::Error::new(error.kind(), error.to_string()))
                            .context("starting Compio IPC");
                    match request {
                        Request::Connect(request) => {
                            let _ = request.connected.send(Err(report));
                        }
                        Request::Setup(request) => {
                            let _ = request.output.send(Err(report));
                        }
                    }
                }
            }
        })?;
    Ok(send)
}

async fn set_up(request: SetupRequest) {
    let SetupRequest {
        command,
        payload,
        limit,
        timeout,
        timeout_context,
        mut output,
    } = request;
    let command = compio::time::timeout(timeout, super::process::output(command, payload, limit));
    let result = tokio::select! {
        result = command => result.context(timeout_context).and_then(|result| result),
        _ = output.closed() => return,
    };
    let _ = output.send(result);
}

async fn serve(request: ConnectRequest) {
    let ConnectRequest {
        target,
        peer,
        capabilities,
        controller,
        mut connected,
    } = request;
    match target {
        Target::Local(path) => {
            let negotiate = async {
                let socket = match peer {
                    PeerRole::Control => SocketRole::Control,
                    PeerRole::Tui => SocketRole::Client,
                    _ => unreachable!("local endpoints only support control and TUI"),
                };
                let stream = compio_connection::open(&path, socket)
                    .await
                    .context("DaemonUnavailable")?;
                compio::time::timeout(
                    COMMAND_TIMEOUT,
                    LocalConnection::establish(stream, peer, capabilities, controller),
                )
                .await
                .context("daemon handshake timed out")?
            };
            let connection = tokio::select! {
                result = negotiate => result,
                _ = connected.closed() => return,
            };
            serve_connected(connection, connected).await;
        }
        Target::Relay(command) => {
            let setup = set_up_relay(command, peer, capabilities, controller);
            let (connection, _child, _diagnostic) = match tokio::select! {
                result = setup => result,
                _ = connected.closed() => return,
            } {
                Ok(relay) => relay,
                Err(error) => {
                    let _ = connected.send(Err(error));
                    return;
                }
            };
            serve_connected(Ok(connection), connected).await;
        }
    }
}

async fn set_up_relay(
    command: Command,
    peer: PeerRole,
    capabilities: Vec<String>,
    controller: Option<String>,
) -> Result<(
    Connection<super::process::ReadPipe, super::process::Pipe>,
    async_process::Child,
    compio::runtime::JoinHandle<Vec<u8>>,
)> {
    use super::process::{Process, classify, drain_diagnostic};

    let Process {
        mut child,
        stdin,
        stdout,
        stderr,
    } = Process::start(command).await?;
    let mut diagnostic = spawn(drain_diagnostic(stderr));
    let result = compio::time::timeout(
        super::RELAY_SETUP_TIMEOUT,
        Connection::negotiate(stdout, stdin, peer, capabilities, controller),
    )
    .await;
    match result {
        Ok(Ok(connection)) => Ok((connection, child, diagnostic)),
        Ok(Err(error)) => Err(classify(error, &mut child, &mut diagnostic).await),
        Err(_) => Err(ProtocolFault::new("Offline", "daemon relay setup timed out").into()),
    }
}

async fn serve_connected<R: AsyncRead, W: AsyncWrite>(
    connection: Result<Connection<R, W>>,
    connected: oneshot::Sender<Result<(Session, ReadQueue, WriteQueue)>>,
) {
    let Connection {
        session,
        mut reader,
        mut writer,
    } = match connection {
        Ok(connection) => connection,
        Err(error) => {
            let _ = connected.send(Err(error));
            return;
        }
    };

    // Keep at most one queued frame per direction. The completion reports
    // actual wire writes so clients retain their existing deadlines.
    let (frames, receive) = mpsc::channel(1);
    let (send, mut writes) = mpsc::channel::<WriteRequest>(1);
    if connected
        .send(Ok((session, receive, WriteQueue(send))))
        .is_err()
    {
        return;
    }

    let upload = async {
        while let Some(WriteRequest {
            message,
            mut complete,
        }) = writes.recv().await
        {
            let result = tokio::select! {
                result = compio::time::timeout(COMMAND_TIMEOUT, writer.envelope(&message)) => {
                    result.unwrap_or(Err(FrameError::Timeout))
                }
                // Cancellation can leave a partially written frame.
                _ = complete.closed() => return,
            };
            let failed = result.is_err();
            if complete.send(result).is_err() || failed {
                return;
            }
        }
    };
    let download = async {
        loop {
            match reader.next_frame().await {
                Ok(Some(frame)) => {
                    if frames.send(Ok(frame)).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = frames.send(Err(error)).await;
                    break;
                }
            }
        }
    };
    tokio::select! {
        () = upload => {}
        () = download => {}
        () = frames.closed() => {}
    }
}

pub(super) async fn send(queue: &WriteQueue, message: Envelope) -> Result<(), FrameError> {
    let (complete, written) = oneshot::channel();
    queue
        .0
        .send(WriteRequest { message, complete })
        .await
        .map_err(|_| closed())?;
    written.await.map_err(|_| closed())?
}

fn closed() -> FrameError {
    io::Error::new(io::ErrorKind::BrokenPipe, "Compio IPC connection closed").into()
}
