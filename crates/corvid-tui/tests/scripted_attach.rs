#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use async_process::{Child, Command};
use compio::net::UnixStream;
use corvid_async::time::{sleep, timeout};
use corvid_protocol::unix::{RuntimeDir, SocketRole};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{Attached, ClipboardWrite, RenderHeader, RouteToken, TerminalExited};
use corvid_protocol::{
    CLIPBOARD_WRITE_CAPABILITY, FrameReader, FrameRole, FrameWriter, PeerRole, RenderFrame,
    accept_hello,
};
use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::pty::{Winsize, openpty};
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(10);
const OUTPUT: &[u8] = b"scripted terminal output";
const OSC52: &[u8] = b"\x1b]52;c;aGVsbG8K7ZWc6riA\x1b\\";
const STALE_OSC52: &[u8] = b"\x1b]52;c;c3RhbGU=\x1b\\";
const AFTER_CLIPBOARD: &[u8] = b"after clipboard";

#[compio::test]
async fn tui_renders_and_routes_input_through_a_scripted_daemon() -> Result<()> {
    scripted_attach(false).await
}

#[compio::test]
async fn tui_relays_only_opted_in_clipboard_writes_for_the_active_route() -> Result<()> {
    scripted_attach(true).await
}

async fn scripted_attach(allow_clipboard: bool) -> Result<()> {
    let home = std::env::home_dir().context("fixture needs a private home directory")?;
    let fixture = tempfile::tempdir_in(home)?;
    let runtime = RuntimeDir::create_all(&fixture.path().join("runtime"))?;
    let instance = runtime.lock_instance()?;
    let listener = instance.bind(SocketRole::Client).await?;

    let (mut child, mut terminal) = spawn_tui(
        runtime.path(),
        &fixture.path().join("views"),
        allow_clipboard,
    )?;

    timeout(WAIT, async {
        let stream = listener.accept().await?.context("rejected TUI peer")?;
        serve(stream, &mut terminal, allow_clipboard).await?;
        let status = child.status().await?;
        ensure!(status.code() == Some(7), "unexpected TUI status: {status}");
        Ok(())
    })
    .await
    .context("waiting for scripted TUI attachment")?
}

fn spawn_tui(runtime: &Path, views: &Path, allow_clipboard: bool) -> Result<(Child, File)> {
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

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_corvid"));
    command
        .arg("--runtime-dir")
        .arg(runtime)
        .arg("attach")
        .env("CORVID_AGENT_MONITOR", "0")
        .env(
            "CORVID_ALLOW_CLIPBOARD_WRITE",
            if allow_clipboard { "1" } else { "0" },
        )
        .env("CORVID_VIEW_STATE_DIR", views);
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
    let child = Command::from(command)
        .stdin(Stdio::from(pty.slave.try_clone()?))
        .stdout(Stdio::from(pty.slave.try_clone()?))
        .stderr(Stdio::from(pty.slave))
        .kill_on_drop(true)
        .spawn()
        .context("starting scripted TUI")?;
    Ok((child, File::from(pty.master)))
}

async fn serve(stream: UnixStream, terminal: &mut File, allow_clipboard: bool) -> Result<()> {
    let (read, write) = corvid_async::io::split_native(stream);
    let mut writer = FrameWriter::start(write, FrameRole::TuiOutput).await?;
    let mut reader = FrameReader::start(read, FrameRole::StructuredOnly).await?;
    let hello = reader.next_envelope().await?.context("missing Hello")?;
    let (session, hello_reply) = accept_hello(
        &hello,
        PeerRole::Tui,
        &Uuid::new_v4().to_string(),
        &Uuid::new_v4().to_string(),
        &[
            "attach.v1",
            "render.v1",
            "paste.v1",
            CLIPBOARD_WRITE_CAPABILITY,
        ],
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
    ensure!(
        allow_clipboard
            == attach
                .terminal_capabilities
                .iter()
                .any(|cap| cap == CLIPBOARD_WRITE_CAPABILITY),
        "clipboard capability does not match the user's opt-in"
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
    terminal_output_until(terminal, OUTPUT).await?;
    terminal.write_all(b"x").context("writing TUI input")?;
    let request = reader.next_envelope().await?.context("missing input")?;
    ensure!(matches!(
        session.validate_request(&request)?,
        Body::InputChunk(chunk)
            if chunk.data == b"x" && chunk.route_token.as_ref() == Some(&route)
    ));

    let stale = RouteToken {
        view_revision: 0,
        ..route.clone()
    };
    for (route, text) in [(stale, "stale"), (route.clone(), "hello\n한글")] {
        writer
            .envelope(&session.envelope(
                None,
                Body::ClipboardWrite(ClipboardWrite {
                    route_token: Some(route),
                    text: text.into(),
                }),
            ))
            .await?;
    }
    writer
        .render(&RenderFrame {
            header: RenderHeader {
                host_id: session.host_id.clone(),
                server_epoch: session.server_epoch.clone(),
                client_id: route.client_id.clone(),
                view_revision: route.view_revision,
                render_generation: 1,
                frame_id: 2,
                base_frame_id: Some(1),
                ansi_bytes: AFTER_CLIPBOARD.len() as u32,
                ..Default::default()
            },
            ansi: AFTER_CLIPBOARD.to_vec(),
        })
        .await?;
    let output = terminal_output_until(terminal, AFTER_CLIPBOARD).await?;
    ensure!(
        !output
            .windows(STALE_OSC52.len())
            .any(|bytes| bytes == STALE_OSC52)
    );
    ensure!(output.windows(OSC52.len()).any(|bytes| bytes == OSC52) == allow_clipboard);

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

async fn terminal_output_until(terminal: &mut File, expected: &[u8]) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 1024];
    while bytes.len() < 4096 {
        match terminal.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                bytes.extend_from_slice(&buffer[..count]);
                if bytes
                    .windows(expected.len())
                    .any(|window| window == expected)
                {
                    return Ok(bytes);
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
