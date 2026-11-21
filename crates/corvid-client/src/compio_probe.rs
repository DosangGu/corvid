use std::path::Path;

use anyhow::{Context, Result, ensure};
use compio::io::{AsyncRead, AsyncWrite, util::Splittable};
use compio::time::timeout;
use corvid_protocol::wire::{Ping, envelope::Body};
use corvid_protocol::{
    COMMAND_TIMEOUT, CompioFrameReader, CompioFrameWriter, FrameError, FrameRole, PeerRole,
    ProtocolFault, check_hello_result, hello,
};
use uuid::Uuid;

use crate::control::check_reply;

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
    let (read, write) = stream.split();
    let (mut reader, mut writer, session) = timeout(COMMAND_TIMEOUT, async {
        let mut writer = CompioFrameWriter::start(write, FrameRole::StructuredOnly).await?;
        let mut reader = match CompioFrameReader::start(read, FrameRole::StructuredOnly).await {
            Ok(reader) => reader,
            Err(FrameError::ProtocolMismatch) => {
                return Err(ProtocolFault::unsupported_framing().into());
            }
            Err(error) => return Err(error.into()),
        };
        let greeting = hello(
            Uuid::new_v4().to_string(),
            Some(Uuid::new_v4().to_string()),
            vec!["ping.v1".to_owned()],
        );
        writer.envelope(&greeting).await?;
        let reply = reader
            .next_envelope()
            .await?
            .context("daemon closed during Hello")?;
        let session = check_hello_result(&greeting, &reply, PeerRole::Control)?;
        Ok::<_, anyhow::Error>((reader, writer, session))
    })
    .await
    .context("daemon handshake timed out")??;
    let reply = timeout(COMMAND_TIMEOUT, async {
        let request = session.envelope(Some(Uuid::new_v4().to_string()), Body::Ping(Ping {}));
        writer.envelope(&request).await?;
        let reply = reader
            .next_envelope()
            .await?
            .context("daemon closed during command")?;
        check_reply(&session, &request, reply)
    })
    .await
    .context("control command timed out")??;
    ensure!(
        matches!(reply, Body::Pong(_)),
        "unexpected response to Ping"
    );
    Ok(())
}
