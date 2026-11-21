use std::io::{self, IsTerminal};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use compio::io::{AsyncRead, AsyncWrite, util::Splittable};
use compio::time::timeout;
use corvid_protocol::{
    COMMAND_TIMEOUT, CompioFrameReader, CompioFrameWriter, Frame, FrameError, FrameRole,
    ProtocolFault,
};

use crate::{connection::SocketRole, relay::bootstrap_reply};

pub async fn compio_relay(runtime: &Path) -> Result<()> {
    relay(runtime, SocketRole::Control, FrameRole::StructuredOnly).await
}

pub async fn compio_relay_tui(runtime: &Path) -> Result<()> {
    relay(runtime, SocketRole::Client, FrameRole::TuiOutput).await
}

async fn relay(runtime: &Path, socket: SocketRole, output: FrameRole) -> Result<()> {
    ensure!(
        !io::stdin().is_terminal() && !io::stdout().is_terminal(),
        "a stdio relay requires a connection without a TTY"
    );
    let mut outgoing = CompioFrameWriter::start(compio::fs::stdout(), output).await?;
    let mut incoming = timeout(
        COMMAND_TIMEOUT,
        CompioFrameReader::start(compio::fs::stdin(), FrameRole::StructuredOnly),
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
    let (read, write) = Splittable::split(stream);
    let mut to_daemon = CompioFrameWriter::start(write, FrameRole::StructuredOnly).await?;
    let mut from_daemon =
        match timeout(COMMAND_TIMEOUT, CompioFrameReader::start(read, output)).await? {
            Ok(reader) => reader,
            Err(FrameError::ProtocolMismatch) => {
                let fault = ProtocolFault::unsupported_framing();
                bootstrap_fault(&mut incoming, &mut outgoing, fault.clone()).await?;
                return Err(fault.into());
            }
            Err(error) => return Err(error.into()),
        };

    let upload = forward(&mut incoming, &mut to_daemon);
    let download = forward(&mut from_daemon, &mut outgoing);
    let mut upload = std::pin::pin!(upload);
    let mut download = std::pin::pin!(download);
    tokio::select! {
        result = &mut download => result,
        result = &mut upload => {
            result?;
            timeout(COMMAND_TIMEOUT, &mut download).await.context("relay drain timed out")?
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn open(runtime: &Path, role: SocketRole) -> Result<compio::net::UnixStream> {
    corvid_protocol::unix::RuntimeDir::open(runtime)?
        .connect_compio(role)
        .await
}

#[cfg(windows)]
async fn open(runtime: &Path, role: SocketRole) -> Result<compio::fs::named_pipe::NamedPipeClient> {
    role.connect_compio(runtime).await
}

async fn forward(
    reader: &mut CompioFrameReader<impl AsyncRead>,
    writer: &mut CompioFrameWriter<impl AsyncWrite>,
) -> Result<()> {
    while let Some(frame) = reader.next_frame().await? {
        timeout(COMMAND_TIMEOUT, async {
            match frame {
                Frame::Structured(payload) => writer.opaque_envelope(payload).await,
                Frame::Render(render) => writer.render(render).await,
            }
        })
        .await??;
    }
    writer.shutdown().await?;
    Ok(())
}

async fn bootstrap_fault(
    incoming: &mut CompioFrameReader<impl AsyncRead>,
    outgoing: &mut CompioFrameWriter<impl AsyncWrite>,
    fault: ProtocolFault,
) -> Result<()> {
    timeout(COMMAND_TIMEOUT, async {
        let request = incoming
            .next_envelope()
            .await?
            .context("missing relay Hello")?;
        outgoing.envelope(&bootstrap_reply(request, fault)?).await?;
        Ok(())
    })
    .await?
}
