use std::future::Future;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use compio::io::util::Splittable;
use compio::runtime::{Runtime, spawn};
use compio::time::{sleep, timeout};
use corvid_async::{mpsc, oneshot, watch};
use corvid_client::ControlEndpoint;
use corvid_protocol::wire::PresentationChanged;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::{COMMAND_TIMEOUT, CompioFrameReader, CompioFrameWriter};
use futures_util::stream::{FuturesUnordered, StreamExt};

use super::{Call, IDLE_TIMEOUT, MAX_PEERS, PresentationAddress, ROLE, Store};

pub(super) async fn run(
    runtime: &PresentationAddress,
    store: Store,
    endpoint: ControlEndpoint,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let runtime = runtime.to_owned();
    let (stop, stopped) = oneshot::channel::<()>();
    let (send_result, mut worker) = oneshot::channel();
    thread::Builder::new()
        .name("corvid-companion".into())
        .spawn(move || {
            let result = Runtime::new()
                .map_err(anyhow::Error::from)
                .and_then(|executor| {
                    executor.block_on(super::serve_runtime(
                        &runtime,
                        store,
                        endpoint,
                        async move {
                            let _ = stopped.await;
                        },
                    ))
                });
            let _ = send_result.send(result);
        })?;
    corvid_async::select! {
        result = &mut worker => result?,
        () = shutdown => {
            drop(stop);
            worker.await?
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
type Listener<'a> = corvid_protocol::unix::CompioBoundSocket<'a>;
#[cfg(windows)]
type Listener<'a> = corvid_async::Mutex<corvid_protocol::windows::CompioPipeListener>;

#[cfg(any(target_os = "linux", target_os = "macos"))]
type Stream = compio::net::UnixStream;
#[cfg(windows)]
type Stream = compio::runtime::fd::AsyncFd<std::os::windows::io::OwnedHandle>;

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn next_peer(listener: &Listener<'_>) -> Result<Option<Stream>> {
    listener.accept().await
}

#[cfg(windows)]
async fn next_peer(listener: &Listener<'_>) -> Result<Option<Stream>> {
    listener.lock().await.accept().await.map(Some)
}

pub(super) async fn accept(
    listener: &Listener<'_>,
    calls: mpsc::Sender<Call>,
    changes: watch::Receiver<u64>,
    host: &str,
    epoch: &str,
) -> Result<()> {
    let mut peers = FuturesUnordered::new();
    let mut incoming = std::pin::pin!(next_peer(listener));
    loop {
        corvid_async::select! {
            result = &mut incoming => {
                incoming.set(next_peer(listener));
                match result {
                    Ok(Some(stream)) if peers.len() < MAX_PEERS => {
                        peers.push(spawn(serve(stream, calls.clone(), changes.clone(), host.to_owned(), epoch.to_owned())));
                    }
                    Ok(_) => {}
                    Err(_) => sleep(Duration::from_millis(25)).await,
                }
            }
            result = peers.next() if !peers.is_empty() => {
                if matches!(result, Some(Ok(Ok(true)))) {
                    return Ok(());
                }
            }
        }
    }
}

async fn serve(
    stream: Stream,
    calls: mpsc::Sender<Call>,
    mut changes: watch::Receiver<u64>,
    host: String,
    epoch: String,
) -> Result<bool> {
    let (read, write) = stream.split();
    let (session, mut reader, mut writer) = timeout(COMMAND_TIMEOUT, async {
        let mut writer = CompioFrameWriter::start(write, ROLE).await?;
        let mut reader = CompioFrameReader::start(read, ROLE).await?;
        let request = reader
            .next_envelope()
            .await?
            .context("missing presentation Hello")?;
        let (session, reply) = match super::negotiate_peer(&request, &host, &epoch) {
            Ok(accepted) => accepted,
            Err(fault) => {
                writer.envelope(&super::rejection(request, &fault)).await?;
                return Err(fault.into());
            }
        };
        writer.envelope(&reply).await?;
        Ok::<_, anyhow::Error>((session, reader, writer))
    })
    .await??;
    while let Some(mut request) = timeout(IDLE_TIMEOUT, reader.next_envelope()).await?? {
        let (body, stop) = match session.validate_request(&request) {
            Ok(Body::WaitPresentationChange(wait)) => {
                let after = wait.after_revision;
                let current = *changes.borrow_and_update();
                let revision = if current > after {
                    current
                } else {
                    let _ = timeout(
                        super::WATCH_WAIT_TIMEOUT,
                        changes.wait_for(|revision| *revision > after),
                    )
                    .await;
                    *changes.borrow_and_update()
                };
                (
                    Body::PresentationChanged(PresentationChanged { revision }),
                    false,
                )
            }
            _ => super::reply(&session, &calls, &mut request).await,
        };
        let reply = session.envelope(request.request_id, body);
        timeout(COMMAND_TIMEOUT, writer.envelope(&reply)).await??;
        if stop {
            return Ok(true);
        }
    }
    Ok(false)
}
