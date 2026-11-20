#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use corvid_protocol::unix::{RuntimeDir, SocketRole};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{Attached, RenderHeader, RouteToken, TerminalExited};
use corvid_protocol::{FrameReader, FrameRole, FrameWriter, PeerRole, RenderFrame, accept_hello};
use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};
use tokio::net::UnixStream;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(10);
const OUTPUT: &[u8] = b"scripted terminal output";

#[tokio::test]
async fn tui_renders_and_routes_input_through_a_scripted_daemon() -> Result<()> {
    let home = std::env::home_dir().context("fixture needs a private home directory")?;
    let fixture = tempfile::tempdir_in(home)?;
    let runtime = RuntimeDir::create_all(&fixture.path().join("runtime"))?;
    let instance = runtime.lock_instance()?;
    let listener = instance.bind(SocketRole::Client).await?;

    let pty = native_pty_system().openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut input = pty.master.take_writer()?;
    let mut output = pty.master.try_clone_reader()?;
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_corvid"));
    command.arg("--runtime-dir");
    command.arg(runtime.path());
    command.arg("attach");
    command.env("CORVID_AGENT_MONITOR", "0");
    command.env("CORVID_VIEW_STATE_DIR", fixture.path().join("views"));
    let mut child = Tui(pty.slave.spawn_command(command)?);
    drop(pty.slave);

    timeout(WAIT, async {
        let stream = listener.accept().await?.context("rejected TUI peer")?;
        serve(stream, &mut input).await?;
        loop {
            if let Some(status) = child.0.try_wait()? {
                ensure!(status.exit_code() == 7, "unexpected TUI status: {status}");
                return Ok::<_, anyhow::Error>(());
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("waiting for scripted TUI attachment")??;

    let (send, receive) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        // Linux PTY masters return EIO when the last slave descriptor closes.
        let _ = output.by_ref().take(4096).read_to_end(&mut bytes);
        let _ = send.send(bytes);
    });
    let bytes = receive.recv_timeout(WAIT).context("reading TUI output")?;
    ensure!(
        bytes.windows(OUTPUT.len()).any(|window| window == OUTPUT),
        "scripted output was missing from the TTY: {:?}",
        String::from_utf8_lossy(&bytes)
    );
    Ok(())
}

async fn serve(stream: UnixStream, input: &mut dyn Write) -> Result<()> {
    let (read, write) = stream.into_split();
    let mut writer = FrameWriter::start(write, FrameRole::TuiOutput).await?;
    let mut reader = FrameReader::start(read, FrameRole::StructuredOnly).await?;
    let hello = reader.next_envelope().await?.context("missing Hello")?;
    let (session, hello_reply) = accept_hello(
        &hello,
        PeerRole::Tui,
        &Uuid::new_v4().to_string(),
        &Uuid::new_v4().to_string(),
        &["attach.v1", "render.v1", "paste.v1"],
    )?;
    writer.envelope(&hello_reply).await?;

    let request = reader.next_envelope().await?.context("missing Attach")?;
    let Body::Attach(attach) = session.validate_request(&request)? else {
        bail!("TUI did not send Attach");
    };
    ensure!(
        attach.launch.is_none(),
        "TUI unexpectedly requested a shell"
    );
    let route = RouteToken {
        client_id: Uuid::new_v4().to_string(),
        connection_generation: 1,
        view_revision: 1,
        terminal_id: Uuid::new_v4().to_string(),
        writer_lease_generation: 1,
    };
    let attached = Attached {
        client_id: route.client_id.clone(),
        connection_generation: route.connection_generation,
        route_token: Some(route.clone()),
        ..Default::default()
    };
    writer
        .envelope(&session.envelope(request.request_id, Body::Attached(attached)))
        .await?;
    writer
        .render(&RenderFrame {
            header: RenderHeader {
                host_id: session.host_id.clone(),
                server_epoch: session.server_epoch.clone(),
                client_id: route.client_id.clone(),
                view_revision: route.view_revision,
                render_generation: 1,
                frame_id: 1,
                ansi_bytes: OUTPUT.len() as u32,
                ..Default::default()
            },
            ansi: OUTPUT.to_vec(),
        })
        .await?;

    let ack = reader
        .next_envelope()
        .await?
        .context("missing FrameApplied")?;
    ensure!(matches!(
        session.validate_request(&ack)?,
        Body::FrameApplied(applied) if applied.frame_id == 1 && applied.render_generation == 1
    ));
    input.write_all(b"x")?;
    input.flush()?;
    let request = reader.next_envelope().await?.context("missing input")?;
    ensure!(matches!(
        session.validate_request(&request)?,
        Body::InputChunk(chunk)
            if chunk.data == b"x" && chunk.route_token.as_ref() == Some(&route)
    ));

    writer
        .envelope(&session.envelope(
            None,
            Body::TerminalExited(TerminalExited {
                terminal_id: route.terminal_id,
                exit_code: 7,
                signal: None,
            }),
        ))
        .await?;
    Ok(())
}

struct Tui(Box<dyn Child + Send + Sync>);

impl Drop for Tui {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
