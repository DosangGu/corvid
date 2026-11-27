#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use async_process::{Child, Command};
use corvid_async::time::{sleep, timeout};
use corvid_protocol::history::{
    command_output, jumpable_commands, last_command_output, validate_history,
};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    Attach, AttachRole, Detach, FrameApplied, LaunchCommand, ReadTerminalHistory, ResizeViewport,
    Viewport,
};
use corvid_protocol::{
    COMMAND_HISTORY_CAPABILITY, Frame, FrameReader, FrameRole, FrameWriter, PeerRole,
    TERMINAL_HISTORY_CAPABILITY, check_hello_result, decode_envelope, hello,
};
use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::pty::{Winsize, openpty};

use common::{WAIT, command, daemon};

#[compio::test]
async fn live_pty_tracks_osc_133_commands_across_reflow_reattach_and_tui_copy() -> Result<()> {
    let home = std::env::home_dir().context("fixture needs a private home directory")?;
    let temporary = tempfile::tempdir_in(home)?;
    let runtime = temporary.path().join("runtime");
    let state = temporary.path().join("state");
    let _daemon = daemon(&runtime, &state).await;

    timeout(WAIT, async {
        // 1. Launch a PTY emitting two OSC 133/633 commands and verify ReadTerminalHistory.
        let mut relay = command(&runtime)
            .arg("terminal-relay")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let mut writer =
            FrameWriter::start(relay.stdin.take().unwrap(), FrameRole::StructuredOnly).await?;
        let mut reader = FrameReader::start(relay.stdout.take().unwrap(), FrameRole::TuiOutput).await?;
        let greeting = hello(
            "history-probe".into(),
            None,
            vec![
                "attach.v1".into(),
                "render.v1".into(),
                TERMINAL_HISTORY_CAPABILITY.into(),
                COMMAND_HISTORY_CAPABILITY.into(),
            ],
        );
        writer.envelope(&greeting).await?;
        let hello_reply = reader.next_envelope().await?.context("missing HelloResult")?;
        let session = check_hello_result(&greeting, &hello_reply, PeerRole::Tui)?;

        let script = concat!(
            "printf '\\033]133;A\\007$ echo wrap\\033]133;C\\007\\nabcdefghijkl\\n\\033]133;D;0\\007'; ",
            "printf '\\033]633;A\\007$ printf lines\\033]633;C\\007\\nline1\\nline2\\n\\033]633;D;0\\007'; ",
            "printf '\\033]133;A\\007$ READY\\n'; ",
            "sleep 15"
        );
        writer
            .envelope(&session.envelope(
                Some("attach".into()),
                Body::Attach(Attach {
                    viewport: Some(Viewport { rows: 12, cols: 10 }),
                    terminal_capabilities: vec![
                        "legacy_vt.v1".into(),
                        COMMAND_HISTORY_CAPABILITY.into(),
                    ],
                    requested_role: AttachRole::Writer as i32,
                    launch: Some(LaunchCommand {
                        argv: vec![b"/bin/sh".to_vec(), b"-c".to_vec(), script.as_bytes().to_vec()],
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            ))
            .await?;

        let mut route = loop {
            match reader.next_frame().await?.context("relay closed")? {
                Frame::Structured(payload) => {
                    let envelope = decode_envelope(&payload)?;
                    session.validate_scope(&envelope)?;
                    if let Some(Body::Attached(attached)) = envelope.body {
                        break attached.route_token;
                    }
                }
                Frame::Render(_) => {}
            }
        }
        .context("missing route token")?;

        // Wait until READY has been rendered into the terminal screen.
        let mut rendered = Vec::new();
        loop {
            let Some(frame) = reader.next_frame().await? else {
                bail!(
                    "relay closed before READY; rendered={:?}",
                    String::from_utf8_lossy(&rendered)
                );
            };
            match frame {
                Frame::Render(render) => {
                    rendered.extend_from_slice(&render.ansi);
                    writer
                        .envelope(&session.envelope(
                            Some(format!("applied-{}", render.header.frame_id)),
                            Body::FrameApplied(FrameApplied {
                                render_generation: render.header.render_generation,
                                frame_id: render.header.frame_id,
                            }),
                        ))
                        .await?;
                    if rendered.windows(5).any(|w| w == b"READY") {
                        break;
                    }
                }
                Frame::Structured(payload) => {
                    let envelope = decode_envelope(&payload)?;
                    match envelope.body {
                        Some(Body::ViewChanged(view)) => {
                            route.view_revision = view.view_revision;
                        }
                        Some(Body::Error(error)) => {
                            bail!("relay error before READY: {} {}", error.code, error.message);
                        }
                        _ => {}
                    }
                }
            }
        }

        // ReadTerminalHistory before resize.
        writer
            .envelope(&session.envelope(
                Some("h1".into()),
                Body::ReadTerminalHistory(ReadTerminalHistory {
                    expected_view_revision: Some(route.view_revision),
                    shell_only: false,
                }),
            ))
            .await?;
        let h1 = next_history(&session, &mut reader, "h1").await?;
        validate_history(&h1).map_err(anyhow::Error::msg)?;
        let jumpable = jumpable_commands(&h1);
        ensure!(jumpable.len() == 2, "expected 2 commands, got {:?}", h1.commands);
        ensure!(command_output(&h1, jumpable[0]).unwrap().text == "abcdefghijkl");
        ensure!(last_command_output(&h1).unwrap().text == "line1\nline2");

        // Resize wider and verify reflow preserves wrapped command output.
        writer
            .envelope(&session.envelope(
                Some("resize".into()),
                Body::ResizeViewport(ResizeViewport {
                    client_id: route.client_id.clone(),
                    connection_generation: route.connection_generation,
                    viewport: Some(Viewport { rows: 12, cols: 40 }),
                }),
            ))
            .await?;
        sleep(Duration::from_millis(220)).await;
        writer
            .envelope(&session.envelope(
                Some("h2".into()),
                Body::ReadTerminalHistory(ReadTerminalHistory {
                    expected_view_revision: Some(route.view_revision + 1),
                    shell_only: false,
                }),
            ))
            .await?;
        let h2 = next_history(&session, &mut reader, "h2").await?;
        validate_history(&h2).map_err(anyhow::Error::msg)?;
        let jumpable2 = jumpable_commands(&h2);
        ensure!(command_output(&h2, jumpable2[0]).unwrap().text == "abcdefghijkl");
        ensure!(last_command_output(&h2).unwrap().text == "line1\nline2");

        // Detach relay before attaching TUI.
        writer
            .envelope(&session.envelope(Some("detach".into()), Body::Detach(Detach {})))
            .await?;
        let _ = relay.status().await;

        // 2. Attach real TUI with CORVID_ALLOW_CLIPBOARD_WRITE=0 and exercise Ctrl+] ? palette actions.
        let (mut tui, mut pty) = spawn_tui(&runtime, &temporary.path().join("views"))?;
        wait_for(&mut pty, b"READY", "waiting for READY on TUI PTY").await?;

        pty.write_all(b"\x1d?")?;
        wait_for(
            &mut pty,
            b"CORVID COMMANDS",
            "waiting for command palette on TUI PTY",
        )
        .await?;
        pty.write_all(b"Previous command\r")?;
        wait_for(&mut pty, b"Command 2/2", "waiting for Command 2/2 on TUI PTY").await?;

        pty.write_all(b"[")?;
        wait_for(&mut pty, b"Command 1/2", "waiting for Command 1/2 on TUI PTY").await?;
        drain_pty(&mut pty).await?;

        pty.write_all(b"\x1b")?;
        wait_for(
            &mut pty,
            b"\x1b[?25h",
            "waiting for live cursor restore after closing history",
        )
        .await?;
        drain_pty(&mut pty).await?;

        pty.write_all(b"\x1d?")?;
        wait_for(
            &mut pty,
            b"CORVID COMMANDS",
            "waiting for command palette for copy on TUI PTY",
        )
        .await?;
        pty.write_all(b"Copy last command output\r")?;
        // Base64 of "line1\nline2" is "bGluZTEKbGluZTI=".
        wait_for(
            &mut pty,
            b"\x1b]52;c;bGluZTEKbGluZTI=\x1b\\",
            "waiting for OSC 52 on TUI PTY",
        )
        .await?;

        let _ = tui.kill();
        let _ = tui.status().await;
        Ok(())
    })
    .await?
}

async fn next_history(
    session: &corvid_protocol::Session,
    reader: &mut FrameReader<async_process::ChildStdout>,
    request_id: &str,
) -> Result<corvid_protocol::wire::TerminalHistory> {
    loop {
        if let Frame::Structured(payload) = reader.next_frame().await?.context("relay closed")? {
            let envelope = decode_envelope(&payload)?;
            session.validate_scope(&envelope)?;
            if envelope.request_id.as_deref() == Some(request_id) {
                match envelope.body {
                    Some(Body::TerminalHistory(history)) => return Ok(history),
                    other => bail!("unexpected history reply: {other:?}"),
                }
            }
        }
    }
}

fn spawn_tui(runtime: &Path, views: &Path) -> Result<(Child, File)> {
    let size = Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = openpty(&size, None).context("opening TUI PTY")?;
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
        .env("CORVID_ALLOW_CLIPBOARD_WRITE", "0")
        .env("CORVID_VIEW_STATE_DIR", views);
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
        .context("starting TUI")?;
    Ok((child, File::from(pty.master)))
}

async fn drain_pty(pty: &mut File) -> Result<()> {
    sleep(Duration::from_millis(50)).await;
    let mut buffer = [0; 4096];
    loop {
        match pty.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error).context("draining TUI PTY"),
        }
    }
}

async fn wait_for(pty: &mut File, expected: &[u8], label: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    match timeout(
        Duration::from_secs(4),
        read_until(pty, expected, &mut bytes),
    )
    .await
    {
        Ok(result) => result.with_context(|| label.to_owned()),
        Err(_) => {
            let tail_start = bytes.len().saturating_sub(1024);
            let tail = String::from_utf8_lossy(&bytes[tail_start..]);
            bail!("{label}: timed out; tail={tail:?}");
        }
    }
}

async fn read_until(pty: &mut File, expected: &[u8], bytes: &mut Vec<u8>) -> Result<Vec<u8>> {
    let mut buffer = [0; 4096];
    loop {
        match pty.read(&mut buffer) {
            Ok(0) => bail!(
                "TUI PTY closed before {:?} appeared",
                String::from_utf8_lossy(expected)
            ),
            Ok(count) => {
                bytes.extend_from_slice(&buffer[..count]);
                if bytes.windows(expected.len()).any(|w| w == expected) {
                    return Ok(std::mem::take(bytes));
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error).context("reading TUI PTY"),
        }
    }
}
