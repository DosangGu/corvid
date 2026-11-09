use std::env;
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::{oneshot, watch};
use vt100::{Callbacks, Parser, Screen};

const HISTORY: usize = 512;
const TICK: Duration = Duration::from_millis(32);
const INPUT_SLOTS: usize = 64;
pub const MAX_INPUT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    pub rows: NonZeroU16,
    pub cols: NonZeroU16,
}

impl Grid {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows: NonZeroU16::new(rows.clamp(2, 100)).unwrap(),
            cols: NonZeroU16::new(cols.clamp(2, 300)).unwrap(),
        }
    }

    fn pty(self) -> PtySize {
        PtySize {
            rows: self.rows.get(),
            cols: self.cols.get(),
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

pub enum State {
    Starting,
    Running,
    Exited(u32),
    Failed(String),
}

pub struct Buffer {
    parser: Parser<Replies>,
    pub state: State,
}

impl Buffer {
    pub fn screen(&self) -> &Screen {
        self.parser.screen()
    }
}

#[derive(Default)]
struct Replies(Vec<u8>);

impl Callbacks for Replies {
    fn unhandled_csi(
        &mut self,
        screen: &mut Screen,
        first: Option<u8>,
        second: Option<u8>,
        params: &[&[u16]],
        action: char,
    ) {
        if first.is_some() || second.is_some() || self.0.len() >= 1024 {
            return;
        }
        match (action, params) {
            ('c', &[] | &[&[0]]) => self.0.extend_from_slice(b"\x1b[?6c"),
            ('n', &[&[5]]) => self.0.extend_from_slice(b"\x1b[0n"),
            ('n', &[&[6]]) => {
                let (row, col) = screen.cursor_position();
                let (_, cols) = screen.size();
                let _ = write!(self.0, "\x1b[{};{}R", row + 1, (col + 1).min(cols.get()));
            }
            _ => {}
        }
    }
}

pub struct Session {
    shared: Arc<Shared>,
    input: SyncSender<Vec<u8>>,
    pub resize: watch::Sender<Grid>,
}

struct Shared {
    buffer: Mutex<Buffer>,
    changes: watch::Sender<()>,
}

impl Shared {
    fn buffer(&self) -> MutexGuard<'_, Buffer> {
        self.buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn update(&self, edit: impl FnOnce(&mut Buffer)) {
        edit(&mut self.buffer());
        self.changes.send_replace(());
    }
}

pub struct Job {
    stop: oneshot::Sender<()>,
    worker: JoinHandle<()>,
}

impl Job {
    pub fn finished(&self) -> bool {
        self.worker.is_finished()
    }

    pub fn request_shutdown(self) -> JoinHandle<()> {
        let _ = self.stop.send(());
        self.worker
    }

    #[cfg(test)]
    fn shutdown(self) {
        let _ = self.request_shutdown().join();
    }
}

impl Session {
    pub fn start(grid: Grid, command: CommandBuilder) -> Result<(Self, Job)> {
        let (changes, _) = watch::channel(());
        let shared = Arc::new(Shared {
            changes,
            buffer: Mutex::new(Buffer {
                parser: Parser::new_with_callbacks(
                    grid.rows,
                    grid.cols,
                    HISTORY,
                    Replies::default(),
                ),
                state: State::Starting,
            }),
        });
        let (input, receive) = sync_channel(INPUT_SLOTS);
        let (resize, sizes) = watch::channel(grid);
        let (stop, stopped) = oneshot::channel();
        let worker = {
            let shared = Arc::clone(&shared);
            let input = input.clone();
            thread::Builder::new()
                .name("corvid-local-pty".into())
                .spawn(
                    move || match run(command, sizes, stopped, receive, input, &shared) {
                        Ok(code) => shared.update(|buffer| buffer.state = State::Exited(code)),
                        Err(error) => shared
                            .update(|buffer| buffer.state = State::Failed(format!("{error:#}"))),
                    },
                )?
        };
        Ok((
            Self {
                shared,
                input,
                resize,
            },
            Job { stop, worker },
        ))
    }

    pub fn buffer(&self) -> MutexGuard<'_, Buffer> {
        self.shared.buffer()
    }

    pub fn changes(&self) -> watch::Receiver<()> {
        self.shared.changes.subscribe()
    }

    pub fn scroll(&self, delta: isize) {
        let mut buffer = self.shared.buffer();
        let screen = buffer.parser.screen_mut();
        let offset = screen.scrollback().saturating_add_signed(delta);
        screen.set_scrollback(offset);
    }

    pub fn send(&self, bytes: Vec<u8>) -> bool {
        if bytes.len() > MAX_INPUT || !matches!(self.buffer().state, State::Running) {
            return false;
        }
        match self.input.try_send(bytes) {
            Ok(()) => {
                self.shared.buffer().parser.screen_mut().set_scrollback(0);
                true
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        }
    }
}

pub fn powershell() -> Result<CommandBuilder> {
    let root =
        PathBuf::from(env::var_os("SystemRoot").context("Windows system directory unavailable")?);
    let executable = root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let mut command = CommandBuilder::new(executable);
    command.arg("-NoLogo");
    command.env("TERM", "xterm-256color");
    if let Some(home) = env::var_os("USERPROFILE") {
        command.cwd(home);
    }
    Ok(command)
}

fn run(
    command: CommandBuilder,
    mut sizes: watch::Receiver<Grid>,
    mut stop: oneshot::Receiver<()>,
    input: Receiver<Vec<u8>>,
    replies: SyncSender<Vec<u8>>,
    shared: &Arc<Shared>,
) -> Result<u32> {
    let grid = *sizes.borrow_and_update();
    let pair = native_pty_system()
        .openpty(grid.pty())
        .context("opening Windows ConPTY")?;
    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    let mut child = pair
        .slave
        .spawn_command(command)
        .context("starting local PowerShell")?;
    drop(pair.slave);

    let closing = Arc::new(AtomicBool::new(false));
    let read = {
        let shared = Arc::clone(shared);
        thread::Builder::new()
            .name("corvid-local-output".into())
            .spawn(move || read_output(reader, &shared, replies))
    };
    let read = match read {
        Ok(read) => read,
        Err(error) => {
            let _ = child.kill();
            return Err(error.into());
        }
    };
    let write = {
        let closing = Arc::clone(&closing);
        thread::Builder::new()
            .name("corvid-local-input".into())
            .spawn(move || write_input(writer, input, &closing))
    };

    let result = match &write {
        Ok(_) => {
            shared.update(|buffer| buffer.state = State::Running);
            (|| -> Result<u32> {
                loop {
                    if let Some(exit) = child.try_wait()? {
                        return Ok(exit.exit_code());
                    }
                    if !matches!(stop.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
                        return Ok(0);
                    }
                    if read.is_finished() {
                        return Err(anyhow!("local terminal output closed"));
                    }
                    if sizes.has_changed().unwrap_or(false) {
                        let grid = *sizes.borrow_and_update();
                        shared.update(|buffer| buffer.parser.set_size(grid.rows, grid.cols));
                        pair.master.resize(grid.pty())?;
                    }
                    thread::sleep(TICK);
                }
            })()
        }
        Err(error) => Err(anyhow!(
            "could not start the terminal input worker: {error}"
        )),
    };
    closing.store(true, Ordering::Release);
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    // Keep the output drain running until ClosePseudoConsole releases the pipe.
    drop(pair.master);
    let _ = read.join();
    if let Ok(write) = write {
        let _ = write.join();
    }
    result
}

fn read_output(mut output: impl Read, shared: &Shared, replies: SyncSender<Vec<u8>>) {
    let mut bytes = [0; 4096];
    loop {
        match output.read(&mut bytes) {
            Ok(0) => break,
            Ok(count) => shared.update(|buffer| {
                buffer.parser.process(&bytes[..count]);
                let answer = std::mem::take(&mut buffer.parser.callbacks_mut().0);
                if !answer.is_empty() {
                    let _ = replies.try_send(answer);
                }
            }),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

fn write_input(mut writer: impl Write, input: Receiver<Vec<u8>>, closing: &AtomicBool) {
    while !closing.load(Ordering::Acquire) {
        match input.recv_timeout(TICK) {
            Ok(bytes) => {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn wait_for(condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(12);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the local terminal"
            );
            thread::sleep(TICK);
        }
    }

    #[test]
    fn runs_local_powershell_and_resizes_the_real_console() {
        let mut command = powershell().unwrap();
        command.arg("-NoProfile");
        let (terminal, job) = Session::start(Grid::new(24, 100), command).unwrap();
        wait_for(|| terminal.buffer().screen().contents().contains("PS "));
        assert!(terminal.send("Write-Output ('CORVID_'+'LOCAL_PTY')\r".as_bytes().to_vec()));
        wait_for(|| {
            terminal
                .buffer()
                .screen()
                .contents()
                .lines()
                .any(|line| line == "CORVID_LOCAL_PTY")
        });

        let size = Grid::new(28, 94);
        terminal.resize.send_replace(size);
        wait_for(|| terminal.buffer().screen().size() == (size.rows, size.cols));
        assert!(terminal.send(
            b"Write-Output (('CORVID_'+'WIDTH_')+$Host.UI.RawUI.WindowSize.Width)\r".to_vec()
        ));
        wait_for(|| {
            terminal
                .buffer()
                .screen()
                .contents()
                .lines()
                .any(|line| line == "CORVID_WIDTH_94")
        });

        assert!(terminal.send(b"exit\r".to_vec()));
        wait_for(|| matches!(terminal.buffer().state, State::Exited(0)));
        job.shutdown();
    }

    #[test]
    fn closing_one_pane_leaves_the_other_shell_interactive() {
        let open = || {
            let mut command = powershell().unwrap();
            command.arg("-NoProfile");
            Session::start(Grid::new(24, 80), command).unwrap()
        };
        let (left, left_job) = open();
        let (right, right_job) = open();
        for terminal in [&left, &right] {
            wait_for(|| terminal.buffer().screen().contents().contains("PS "));
        }
        assert!(left.send(b"Write-Output ('CORVID_'+'LEFT_ONLY')\r".to_vec()));
        wait_for(|| {
            left.buffer()
                .screen()
                .contents()
                .lines()
                .any(|line| line == "CORVID_LEFT_ONLY")
        });
        assert!(
            !right
                .buffer()
                .screen()
                .contents()
                .contains("CORVID_LEFT_ONLY")
        );
        left_job.shutdown();

        assert!(right.send(b"Write-Output ('CORVID_'+'RIGHT_ONLY')\r".to_vec()));
        wait_for(|| {
            right
                .buffer()
                .screen()
                .contents()
                .lines()
                .any(|line| line == "CORVID_RIGHT_ONLY")
        });
        right_job.shutdown();
    }
}
