use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread;

use anyhow::{Context, Result};
use compio::runtime::Runtime;
use corvid_protocol::wire::Envelope;
use corvid_protocol::{COMMAND_TIMEOUT, Frame, FrameError, PeerRole, Session};
use tokio::sync::{mpsc, oneshot};

use crate::compio_connection::{self, LocalConnection};
use crate::connection::SocketRole;

pub(super) type ReadQueue = mpsc::Receiver<Result<Frame, FrameError>>;
pub(crate) struct WriteQueue(mpsc::Sender<WriteRequest>);

struct WriteRequest {
    message: Envelope,
    complete: oneshot::Sender<Result<(), FrameError>>,
}

struct ConnectRequest {
    path: PathBuf,
    peer: PeerRole,
    capabilities: Vec<String>,
    controller: Option<String>,
    connected: oneshot::Sender<Result<(Session, ReadQueue, WriteQueue)>>,
}

static DRIVER: OnceLock<io::Result<mpsc::Sender<ConnectRequest>>> = OnceLock::new();

pub(super) async fn connect(
    path: &Path,
    peer: PeerRole,
    capabilities: Vec<String>,
    controller: Option<String>,
) -> Result<(Session, ReadQueue, WriteQueue)> {
    let driver = DRIVER
        .get_or_init(start_driver)
        .as_ref()
        .map_err(|error| io::Error::new(error.kind(), error.to_string()))
        .context("starting Compio IPC thread")?;
    let (connected, ready) = oneshot::channel();
    driver
        .send(ConnectRequest {
            path: path.to_owned(),
            peer,
            capabilities,
            controller,
            connected,
        })
        .await
        .context("Compio IPC driver is unavailable")?;
    ready.await.context("Compio IPC driver stopped")?
}

fn start_driver() -> io::Result<mpsc::Sender<ConnectRequest>> {
    let (send, mut requests) = mpsc::channel::<ConnectRequest>(32);
    thread::Builder::new()
        .name("corvid-ipc".into())
        .spawn(move || match Runtime::new() {
            Ok(runtime) => runtime.block_on(async {
                while let Some(request) = requests.recv().await {
                    compio::runtime::spawn(serve(request)).detach();
                }
            }),
            Err(error) => {
                while let Some(request) = requests.blocking_recv() {
                    let report = io::Error::new(error.kind(), error.to_string());
                    let _ = request
                        .connected
                        .send(Err(report).context("starting Compio IPC"));
                }
            }
        })?;
    Ok(send)
}

async fn serve(request: ConnectRequest) {
    let ConnectRequest {
        path,
        peer,
        capabilities,
        controller,
        mut connected,
    } = request;
    let negotiation = async {
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
        result = negotiation => result,
        _ = connected.closed() => return,
    };
    let LocalConnection {
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
