use std::io::{self, IsTerminal};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use corvid_protocol::unix::SocketRole;
use corvid_protocol::wire::Envelope;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::{
    COMMAND_TIMEOUT, Frame, FrameReader, FrameRole, FrameWriter, PROTOCOL_MAJOR, PROTOCOL_MINOR,
    ProtocolFault,
};
use tokio::io::{AsyncRead, AsyncWrite, stdin, stdout};
use tokio::time::timeout;

pub async fn relay(runtime: &Path) -> Result<()> {
    ensure!(
        !io::stdin().is_terminal() && !io::stdout().is_terminal(),
        "control --stdio requires a connection without a TTY"
    );
    let mut outgoing = FrameWriter::start(stdout(), FrameRole::StructuredOnly).await?;
    let mut incoming = timeout(
        COMMAND_TIMEOUT,
        FrameReader::start(stdin(), FrameRole::StructuredOnly),
    )
    .await
    .context("relay preamble timed out")??;

    let stream = match crate::connection::open(runtime, SocketRole::Control).await {
        Ok(stream) => stream,
        Err(error) => {
            timeout(COMMAND_TIMEOUT, unavailable(&mut incoming, &mut outgoing)).await??;
            return Err(error);
        }
    };
    let (read, write) = stream.into_split();
    let mut to_daemon = FrameWriter::start(write, FrameRole::StructuredOnly).await?;
    let mut from_daemon = timeout(
        COMMAND_TIMEOUT,
        FrameReader::start(read, FrameRole::StructuredOnly),
    )
    .await??;

    let upload = forward(&mut incoming, &mut to_daemon);
    let download = forward(&mut from_daemon, &mut outgoing);
    tokio::pin!(upload, download);
    tokio::select! {
        result = &mut download => result,
        result = &mut upload => {
            result?;
            timeout(COMMAND_TIMEOUT, &mut download).await.context("relay drain timed out")?
        }
    }
}

async fn forward(
    reader: &mut FrameReader<impl AsyncRead + Unpin>,
    writer: &mut FrameWriter<impl AsyncWrite + Unpin>,
) -> Result<()> {
    while let Some(frame) = reader.next_frame().await? {
        let Frame::Structured(payload) = frame else {
            bail!("unexpected render frame on control relay");
        };
        timeout(COMMAND_TIMEOUT, writer.opaque_envelope(&payload)).await??;
    }
    writer.shutdown().await?;
    Ok(())
}

async fn unavailable(
    incoming: &mut FrameReader<impl AsyncRead + Unpin>,
    outgoing: &mut FrameWriter<impl AsyncWrite + Unpin>,
) -> Result<()> {
    let request = incoming
        .next_envelope()
        .await?
        .context("missing relay Hello")?;
    ensure!(
        matches!(request.body, Some(Body::Hello(_))),
        "expected relay Hello"
    );
    let mut error = ProtocolFault::new("DaemonUnavailable", "start the daemon before connecting");
    error.retryable = true;
    let reply = Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        request_id: request
            .request_id
            .filter(|id| id.len() <= 256 && !id.chars().any(char::is_control)),
        body: Some(Body::Error(error.into_wire())),
        ..Default::default()
    };
    outgoing.envelope(&reply).await?;
    Ok(())
}
