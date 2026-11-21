use std::io;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, mpsc as blocking};
use std::thread;

use anyhow::{Context, Result, ensure};
use compio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use compio::runtime::{Runtime, spawn};
use compio::time::{sleep, timeout};
use corvid_async::mpsc;
use corvid_protocol::{COMMAND_TIMEOUT, RENDER_LIMIT};
use prost::Message;

use super::{Request, Response, TICK};

#[cfg(any(target_os = "linux", target_os = "macos"))]
type Stream = compio::net::UnixStream;
#[cfg(windows)]
type Stream = compio::fs::named_pipe::NamedPipeClient;

type Sender = blocking::SyncSender<Result<Response>>;

struct Open {
    directory: PathBuf,
    starting: bool,
    calls: mpsc::Receiver<(Request, Sender)>,
    ready: blocking::SyncSender<Result<()>>,
}

pub(super) struct ClientConnection {
    calls: mpsc::Sender<(Request, Sender)>,
}

static DRIVER: OnceLock<io::Result<mpsc::Sender<Open>>> = OnceLock::new();

impl ClientConnection {
    pub(super) fn connect(directory: PathBuf, starting: bool) -> Result<Self> {
        let driver = DRIVER
            .get_or_init(start_driver)
            .as_ref()
            .map_err(|error| io::Error::new(error.kind(), error.to_string()))
            .context("starting the terminal host driver")?;
        let (calls, incoming) = mpsc::channel(1);
        let (ready, opened) = blocking::sync_channel(1);
        driver
            .send_blocking(Open {
                directory,
                starting,
                calls: incoming,
                ready,
            })
            .context("terminal host driver stopped")?;
        opened.recv().context("terminal host driver stopped")??;
        Ok(Self { calls })
    }

    pub(super) fn call(&mut self, request: Request) -> Result<Response> {
        let (reply, response) = blocking::sync_channel(1);
        self.calls
            .send_blocking((request, reply))
            .context("terminal host connection closed")?;
        response.recv().context("terminal host connection closed")?
    }
}

fn start_driver() -> io::Result<mpsc::Sender<Open>> {
    let (send, mut incoming) = mpsc::channel::<Open>(32);
    thread::Builder::new()
        .name("corvid-terminal-host".into())
        .spawn(move || match Runtime::new() {
            Ok(runtime) => runtime.block_on(async {
                while let Some(open) = incoming.recv().await {
                    spawn(service(open)).detach();
                }
            }),
            Err(error) => {
                while let Some(open) = incoming.blocking_recv() {
                    let report = io::Error::new(error.kind(), error.to_string());
                    let _ = open
                        .ready
                        .send(Err(report).context("starting terminal host driver"));
                }
            }
        })?;
    Ok(send)
}

async fn service(open: Open) {
    let Open {
        directory,
        starting,
        mut calls,
        ready,
    } = open;
    let connection = timeout(COMMAND_TIMEOUT, async {
        loop {
            let stream = connect(&directory).await;
            if stream.is_ok() || !starting {
                return stream;
            }
            sleep(TICK).await;
        }
    })
    .await
    .context("opening terminal host timed out")
    .and_then(|stream| stream);
    let mut stream = match connection {
        Ok(stream) if ready.send(Ok(())).is_ok() => stream,
        Ok(_) => return,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    while let Some((request, reply)) = calls.recv().await {
        let result = timeout(COMMAND_TIMEOUT, async {
            write_message(&mut stream, request).await?;
            read_message(&mut stream).await
        })
        .await
        .context("terminal host request timed out")
        .and_then(|response| response);
        let failed = result.is_err();
        if reply.send(result).is_err() || failed {
            return;
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn connect(path: &Path) -> Result<Stream> {
    use corvid_protocol::unix::{RuntimeDir, SocketRole};

    RuntimeDir::open(path)?
        .connect_compio(SocketRole::TerminalHost)
        .await
}

#[cfg(windows)]
async fn connect(path: &Path) -> Result<Stream> {
    corvid_protocol::windows::DaemonPipe::TerminalHost
        .connect_compio(path)
        .await
}

pub(super) async fn read_message<M: Message + Default>(reader: &mut impl AsyncRead) -> Result<M> {
    let compio::BufResult(result, size) = reader.read_exact([0; 4]).await;
    result?;
    let size = u32::from_be_bytes(size) as usize;
    ensure!(size <= RENDER_LIMIT, "terminal host frame exceeds limit");
    let compio::BufResult(result, bytes) = reader.read_exact(vec![0; size]).await;
    result?;
    Ok(M::decode(bytes.as_slice())?)
}

pub(super) async fn write_message(
    writer: &mut impl AsyncWrite,
    message: impl Message,
) -> Result<()> {
    let size = message.encoded_len();
    ensure!(size <= RENDER_LIMIT, "terminal host frame exceeds limit");
    let mut bytes = Vec::with_capacity(4 + size);
    bytes.extend((size as u32).to_be_bytes());
    message.encode(&mut bytes)?;
    writer.write_all(bytes).await.0?;
    writer.flush().await?;
    Ok(())
}
