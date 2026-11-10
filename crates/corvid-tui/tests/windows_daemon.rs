#![cfg(windows)]

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::num::NonZeroU16;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::process::{Child as Process, Command, Stdio};
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use corvid_protocol::windows::DaemonPipe;
use corvid_protocol::wire::snapshot_entity::Body;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
};

const CORVID: &str = env!("CARGO_BIN_EXE_corvid");
const WAIT: Duration = Duration::from_secs(12);
const ROWS: u16 = 30;
const COLS: u16 = 100;

#[tokio::test]
async fn retains_the_windows_shell_across_tui_detach() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let runtime = fixture.path().join("run");
    let state = fixture.path().join("state");
    let _daemon = Daemon(
        Command::new(CORVID)
            .arg("--runtime-dir")
            .arg(&runtime)
            .args(["daemon", "--state-dir"])
            .arg(state)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    tokio::time::timeout(WAIT, async {
        while corvid_client::ping(&runtime).await.is_err() {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .context("Windows daemon did not answer ping")?;
    let host = corvid_client::host(&runtime).await?;

    let powershell = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .context("SystemRoot is unavailable")?
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let shell = [
        OsString::from("attach"),
        "--new".into(),
        "--".into(),
        powershell.into_os_string(),
        "-NoLogo".into(),
        "-NoProfile".into(),
    ];
    let mut first = Tui::open(&runtime, shell.clone())?;
    first.wait_for("PS ")?;
    first.send(
        b"$env:CORVID_TEST_VALUE='retained'; [Console]::WriteLine(('CORVID_' + [char]0xD55C))\r",
    )?;
    first.wait_for("CORVID_한")?;
    let original = terminal_id(&runtime).await?;

    let mut contender = Tui::open(&runtime, [])?;
    contender.wait_for("workspace already has a writer")?;
    contender.expect_exit(1)?;

    first.send(b"\x1dd")?;
    first.expect_exit(0)?;
    ensure!(
        terminal_id(&runtime).await? == original,
        "detach removed the live terminal"
    );

    let mut second = Tui::open(&runtime, [])?;
    second.wait_for("PS ")?;
    second.send(b"[Console]::WriteLine(('CORVID_' + $env:CORVID_TEST_VALUE))\r")?;
    second.wait_for("CORVID_retained")?;
    ensure!(
        terminal_id(&runtime).await? == original,
        "reattach changed the terminal ID"
    );
    let current = corvid_client::host(&runtime).await?;
    ensure!(
        current.server_epoch == host.server_epoch,
        "reattach changed the daemon epoch"
    );

    second.resize(92, 26)?;
    tokio::time::timeout(WAIT, async {
        loop {
            let snapshot = corvid_client::snapshot(&runtime).await?;
            if snapshot.snapshot().entities().any(|entity| {
                matches!(&entity.body, Some(Body::Client(client))
                    if client.viewport.as_ref().is_some_and(|size| size.cols == 92 && size.rows == 26))
            }) {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .context("Windows daemon did not apply the TUI resize")??;
    second.send(b"[Console]::WriteLine(('CORVID_' + [Console]::WindowWidth + 'x' + [Console]::WindowHeight))\r")?;
    second.wait_for("CORVID_92x25")?;

    second.send(b"\x1dt")?;
    wait_for_layout(&runtime, 2, &original, false).await?;
    second.send(b"\x1d%")?;
    wait_for_layout(&runtime, 3, &original, false).await?;
    second.send(b"\x1d1")?;
    wait_for_layout(&runtime, 3, &original, true).await?;

    second.send(b"\x1d2")?;
    wait_for_layout(&runtime, 3, &original, false).await?;
    second.send(b"\x1dX")?;
    wait_for_layout(&runtime, 2, &original, false).await?;
    second.send(b"\x1dT")?;
    wait_for_layout(&runtime, 1, &original, true).await?;

    second.send(b"exit 7\r")?;
    second.expect_exit(7)?;

    let mut third = Tui::open(&runtime, shell)?;
    third.wait_for("PS ")?;
    third.send(b"\x1dS")?;
    third.expect_exit(0)?;
    let snapshot = corvid_client::snapshot(&runtime).await?;
    ensure!(
        !snapshot
            .snapshot()
            .entities()
            .any(|entity| matches!(entity.body, Some(Body::Terminal(_)))),
        "closing the session left a live terminal"
    );
    Ok(())
}

async fn wait_for_layout(
    runtime: &Path,
    terminal_count: usize,
    original: &str,
    original_selected: bool,
) -> Result<()> {
    tokio::time::timeout(WAIT, async {
        loop {
            let snapshot = corvid_client::snapshot(runtime).await?;
            let mut count = 0;
            let mut selected = None;
            for entity in snapshot.snapshot().entities() {
                match &entity.body {
                    Some(Body::Terminal(_)) => count += 1,
                    Some(Body::Client(client)) => selected = client.terminal_id.as_deref(),
                    _ => {}
                }
            }
            if count == terminal_count
                && selected.is_some_and(|id| (id == original) == original_selected)
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .context("Windows tab or pane transition did not finish")?
}

#[tokio::test]
async fn ensure_daemon_reuses_the_detached_windows_process() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let runtime = fixture.path().join("run");
    let state = fixture.path().join("state");
    let (first, second) = tokio::join!(
        ensure_daemon(&runtime, &state),
        ensure_daemon(&runtime, &state),
    );
    if first.is_err() && second.is_err() {
        return first;
    }
    let daemon = BackgroundDaemon::open(&runtime).await?;
    first?;
    second?;
    let host = corvid_client::host(&runtime).await?;
    ensure_daemon(&runtime, &state).await?;
    let reused = corvid_client::host(&runtime).await?;
    ensure!(
        reused.server_epoch == host.server_epoch,
        "ensure-daemon replaced a live daemon"
    );

    drop(daemon);
    ensure_daemon(&runtime, &state).await?;
    let _restarted = BackgroundDaemon::open(&runtime).await?;
    let restarted = corvid_client::host(&runtime).await?;
    ensure!(
        restarted.host_id == host.host_id,
        "restart changed the host identity"
    );
    ensure!(
        restarted.server_epoch != host.server_epoch,
        "restart reused a daemon epoch"
    );
    Ok(())
}

async fn ensure_daemon(runtime: &Path, state: &Path) -> Result<()> {
    let status = tokio::process::Command::new(CORVID)
        .arg("--runtime-dir")
        .arg(runtime)
        .args(["ensure-daemon", "--state-dir"])
        .arg(state)
        .status()
        .await?;
    ensure!(status.success(), "ensure-daemon failed ({status})");
    Ok(())
}

struct BackgroundDaemon(OwnedHandle);

impl BackgroundDaemon {
    async fn open(runtime: &Path) -> Result<Self> {
        let pipe = DaemonPipe::Control.connect(runtime).await?;
        let mut pid = 0;
        // The connected private pipe remains live while Windows reads its server PID.
        if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        // This PID comes from the isolated fixture's authenticated control pipe.
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        // OpenProcess returned an owned handle.
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle) }))
    }
}

impl Drop for BackgroundDaemon {
    fn drop(&mut self) {
        // The handle refers to this fixture's daemon even if its PID is recycled.
        unsafe {
            TerminateProcess(self.0.as_raw_handle(), 1);
            WaitForSingleObject(self.0.as_raw_handle(), 5000);
        }
    }
}

async fn terminal_id(runtime: &Path) -> Result<String> {
    let state = corvid_client::snapshot(runtime).await?;
    let mut ids = state
        .snapshot()
        .entities()
        .filter_map(|entity| match &entity.body {
            Some(Body::Terminal(terminal)) => Some(terminal.terminal_id.clone()),
            _ => None,
        });
    let id = ids.next().context("daemon has no live terminal")?;
    ensure!(ids.next().is_none(), "daemon has multiple live terminals");
    Ok(id)
}

struct Daemon(Process);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Tui {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    input: Box<dyn Write + Send>,
    output: Option<Receiver<Vec<u8>>>,
    screen: vt100::Parser,
}

impl Tui {
    fn open(runtime: &Path, args: impl IntoIterator<Item = OsString>) -> Result<Self> {
        let pair = native_pty_system().openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut reader = pair.master.try_clone_reader()?;
        let input = pair.master.take_writer()?;
        let (sender, output) = sync_channel(16);
        std::thread::spawn(move || {
            let mut bytes = [0; 4096];
            let mut connected = true;
            while let Ok(count @ 1..) = reader.read(&mut bytes) {
                if connected {
                    connected = sender.send(bytes[..count].to_vec()).is_ok();
                }
            }
        });
        let mut command = CommandBuilder::new(CORVID);
        command.arg("--runtime-dir");
        command.arg(runtime);
        command.args(args);
        let child = pair.slave.spawn_command(command)?;
        let mut tui = Self {
            child,
            master: pair.master,
            input,
            output: Some(output),
            screen: vt100::Parser::new(
                NonZeroU16::new(ROWS).unwrap(),
                NonZeroU16::new(COLS).unwrap(),
                0,
            ),
        };
        tui.answer_cursor_query()?;
        Ok(tui)
    }

    fn answer_cursor_query(&mut self) -> Result<()> {
        let deadline = Instant::now() + WAIT;
        let mut tail = Vec::new();
        loop {
            let bytes = self
                .read(deadline)
                .context("ConPTY did not request cursor position")?;
            tail.extend_from_slice(&bytes);
            if tail.windows(4).any(|window| window == b"\x1b[6n") {
                return self.send(b"\x1b[1;1R");
            }
            let discard = tail.len().saturating_sub(3);
            tail.drain(..discard);
        }
    }

    fn send(&mut self, input: &[u8]) -> Result<()> {
        self.input.write_all(input)?;
        self.input.flush()?;
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.master.resize(PtySize {
            cols,
            rows,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.screen.set_size(
            NonZeroU16::new(rows).unwrap(),
            NonZeroU16::new(cols).unwrap(),
        );
        Ok(())
    }

    fn read(&self, deadline: Instant) -> Result<Vec<u8>> {
        let time = deadline.saturating_duration_since(Instant::now());
        Ok(self.output.as_ref().unwrap().recv_timeout(time)?)
    }

    fn wait_for(&mut self, marker: &str) -> Result<()> {
        let deadline = Instant::now() + WAIT;
        while !self.screen.screen().contents().contains(marker) {
            let output = self
                .read(deadline)
                .with_context(|| format!("waiting for {marker}"))?;
            self.screen.process(&output);
        }
        Ok(())
    }

    fn expect_exit(&mut self, expected: u32) -> Result<()> {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                ensure!(
                    status.exit_code() == expected,
                    "unexpected TUI exit status {status}"
                );
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "TUI did not exit");
            let _ = self.read(Instant::now() + Duration::from_millis(25));
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        drop(self.output.take());
    }
}
