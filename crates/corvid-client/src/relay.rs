use std::io::{self, IsTerminal};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use corvid_protocol::wire::Envelope;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::{
    COMMAND_TIMEOUT, Frame, FrameReader, FrameRole, FrameWriter, PROTOCOL_MAJOR, PROTOCOL_MINOR,
    ProtocolFault,
};
use tokio::io::{AsyncRead, AsyncWrite, stdin, stdout};
use tokio::time::timeout;

use crate::connection::{SocketRole, open, read_preamble};

pub async fn relay(runtime: &Path) -> Result<()> {
    relay_role(runtime, SocketRole::Control, FrameRole::StructuredOnly).await
}

pub async fn relay_tui(runtime: &Path) -> Result<()> {
    relay_role(runtime, SocketRole::Client, FrameRole::TuiOutput).await
}

async fn relay_role(runtime: &Path, socket: SocketRole, output: FrameRole) -> Result<()> {
    ensure!(
        !io::stdin().is_terminal() && !io::stdout().is_terminal(),
        "a stdio relay requires a connection without a TTY"
    );
    let mut outgoing = FrameWriter::start(stdout(), output).await?;
    let mut incoming = timeout(
        COMMAND_TIMEOUT,
        FrameReader::start(stdin(), FrameRole::StructuredOnly),
    )
    .await
    .context("relay preamble timed out")??;

    let stream = match open(runtime, socket).await {
        Ok(stream) => stream,
        Err(error) => {
            let mut fault =
                ProtocolFault::new("DaemonUnavailable", "start the daemon before connecting");
            fault.retryable = true;
            bootstrap_fault(&mut incoming, &mut outgoing, fault).await?;
            return Err(error);
        }
    };
    #[cfg(target_os = "linux")]
    let (read, write) = stream.into_split();
    #[cfg(windows)]
    let (read, write) = tokio::io::split(stream);
    let mut to_daemon = FrameWriter::start(write, FrameRole::StructuredOnly).await?;
    let mut from_daemon = match timeout(COMMAND_TIMEOUT, read_preamble(read, output)).await? {
        Ok(reader) => reader,
        Err(error) => {
            if let Some(fault) = error.downcast_ref::<ProtocolFault>() {
                bootstrap_fault(&mut incoming, &mut outgoing, fault.clone()).await?;
            }
            return Err(error);
        }
    };

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
        timeout(COMMAND_TIMEOUT, async {
            match frame {
                Frame::Structured(payload) => writer.opaque_envelope(&payload).await,
                Frame::Render(render) => writer.render(&render).await,
            }
        })
        .await??;
    }
    writer.shutdown().await?;
    Ok(())
}

async fn bootstrap_fault(
    incoming: &mut FrameReader<impl AsyncRead + Unpin>,
    outgoing: &mut FrameWriter<impl AsyncWrite + Unpin>,
    fault: ProtocolFault,
) -> Result<()> {
    timeout(COMMAND_TIMEOUT, async {
        let request = incoming
            .next_envelope()
            .await?
            .context("missing relay Hello")?;
        ensure!(
            matches!(request.body, Some(Body::Hello(_))),
            "expected relay Hello"
        );
        let reply = Envelope {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            request_id: request
                .request_id
                .filter(|id| id.len() <= 256 && !id.chars().any(char::is_control)),
            body: Some(Body::Error(fault.into_wire())),
            ..Default::default()
        };
        outgoing.envelope(&reply).await?;
        Ok(())
    })
    .await?
}
