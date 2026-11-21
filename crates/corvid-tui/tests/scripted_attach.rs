#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use corvid_async::time::{sleep, timeout};
use corvid_protocol::unix::{RuntimeDir, SocketRole};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{Attached, RenderHeader, RouteToken, TerminalExited};
use corvid_protocol::{FrameReader, FrameRole, FrameWriter, PeerRole, RenderFrame, accept_hello};
use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::pty::{Winsize, openpty};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
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

    let (mut child, mut terminal) = spawn_tui(runtime.path(), &fixture.path().join("views"))?;

    timeout(WAIT, async {
        let stream = listener.accept().await?.context("rejected TUI peer")?;
        serve(stream, &mut terminal).await?;
        let status = child.wait().await?;
        ensure!(status.code() == Some(7), "unexpected TUI status: {status}");
        Ok(())
    })
    .await
    .context("waiting for scripted TUI attachment")?
}

fn spawn_tui(runtime: &Path, views: &Path) -> Result<(Child, File)> {
    let size = Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = openpty(&size, None).context("opening scripted TUI PTY")?;
    fcntl(&pty.master, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    fcntl(&pty.slave, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    let flags = OFlag::from_bits_retain(fcntl(&pty.master, FcntlArg::F_GETFL)?);
    fcntl(&pty.master, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;

    let mut command = Command::new(env!("CARGO_BIN_EXE_corvid"));
    command
        .arg("--runtime-dir")
        .arg(runtime)
        .arg("attach")
        .env("CORVID_AGENT_MONITOR", "0")
        .env("CORVID_VIEW_STATE_DIR", views)
        .stdin(Stdio::from(pty.slave.try_clone()?))
        .stdout(Stdio::from(pty.slave.try_clone()?))
        .stderr(Stdio::from(pty.slave))
        .kill_on_drop(true);
    // Detach from the test runner's controlling terminal. The child uses the
    // PTY through stdio. Only call async-signal-safe operations after fork.
    unsafe {
        command.pre_exec(|| {
            if nix::libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().context("starting scripted TUI")?;
    Ok((child, File::from(pty.master)))
}

async fn serve(stream: UnixStream, terminal: &mut File) -> Result<()> {
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
    expect_rendered_output(terminal).await?;
    terminal.write_all(b"x").context("writing TUI input")?;
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

async fn expect_rendered_output(terminal: &mut File) -> Result<()> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 1024];
    while bytes.len() < 4096 {
        match terminal.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                bytes.extend_from_slice(&buffer[..count]);
                if bytes.windows(OUTPUT.len()).any(|window| window == OUTPUT) {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error).context("reading TUI output"),
        }
    }
    bail!(
        "scripted output was missing from the TTY: {:?}",
        String::from_utf8_lossy(&bytes)
    );
}
