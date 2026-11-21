use std::path::Path;

use anyhow::{Context, Result, ensure};
use compio::time::{sleep, timeout};
use compio_io::{AsyncRead, AsyncWrite, compat::AsyncStream, util::Splittable};
use corvid_protocol::wire::{Ping, envelope::Body};
use corvid_protocol::{COMMAND_TIMEOUT, PeerRole};
use tokio_util::compat::FuturesAsyncReadCompatExt;
use uuid::Uuid;

use crate::{connection::Connection, control::exchange_wire};

/// Probe a running Corvid daemon. Run this future on a Compio runtime.
pub async fn compio_ping(path: &Path) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let stream = {
        use corvid_protocol::unix::{RuntimeDir, SocketRole};

        RuntimeDir::open(path)?
            .connect_compio(SocketRole::Control)
            .await?
    };
    #[cfg(windows)]
    let stream = corvid_protocol::windows::DaemonPipe::Control
        .connect_compio(path)
        .await?;

    ping(stream).await
}

async fn ping<S, R, W>(stream: S) -> Result<()>
where
    S: Splittable<ReadHalf = R, WriteHalf = W> + 'static,
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let stream = Box::pin(AsyncStream::new(stream)).compat();
    let (read, write) = tokio::io::split(stream);
    let mut wire = timeout(
        COMMAND_TIMEOUT,
        Connection::negotiate_with_deadline(
            read,
            write,
            PeerRole::Control,
            vec!["ping.v1".to_owned()],
            Some(Uuid::new_v4().to_string()),
            || sleep(COMMAND_TIMEOUT),
        ),
    )
    .await
    .context("daemon handshake timed out")??;
    let reply = timeout(
        COMMAND_TIMEOUT,
        exchange_wire(
            &mut wire,
            Body::Ping(Ping {}),
            Uuid::new_v4().to_string(),
            || sleep(COMMAND_TIMEOUT),
        ),
    )
    .await
    .context("control command timed out")??;
    ensure!(
        matches!(reply, Body::Pong(_)),
        "unexpected response to Ping"
    );
    Ok(())
}
