use std::io::{self, Cursor, ErrorKind, Read, Write};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::time::{Duration, Instant};

use crate::native_process::ProcessKey;
use anyhow::Result;
use corvid_core::Viewport;
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};

const CHUNK: usize = 8192;
const OUTPUT_CHUNKS: usize = 16;
const FENCE_TIMEOUT: Duration = Duration::from_millis(250);
const EXIT_DRAIN: Duration = Duration::from_millis(100);

pub struct TerminalPty {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    output: Option<Receiver<io::Result<Vec<u8>>>>,
    chunk: Cursor<Vec<u8>>,
    input: Option<SyncSender<Vec<u8>>>,
    completion: Receiver<io::Result<usize>>,
    writing: bool,
    exited: Option<(ExitStatus, Instant)>,
}

impl TerminalPty {
    pub fn spawn(command: CommandBuilder, viewport: Viewport) -> Result<Self> {
        let pair = native_pty_system().openpty(size(viewport))?;
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let output = output_thread(reader)?;
        let child = pair.slave.spawn_command(command)?;
        let (input, completion) = match input_thread(writer) {
            Ok(pipes) => pipes,
            Err(error) => {
                let mut child = child;
                let _ = child.kill();
                return Err(error.into());
            }
        };
        Ok(Self {
            master: pair.master,
            child,
            output: Some(output),
            chunk: Cursor::default(),
            input: Some(input),
            completion,
            writing: false,
            exited: None,
        })
    }

    pub fn resize(&self, viewport: Viewport) -> Result<()> {
        self.master.resize(size(viewport))
    }

    pub(crate) fn process_key(&self) -> Option<ProcessKey> {
        ProcessKey::read(self.child.process_id()?, self.child.as_raw_handle()?)
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.exited.is_none()
            && let Some(status) = self.child.try_wait()?
        {
            self.exited = Some((status, Instant::now()));
        }
        // ConPTY can deliver the final VT data after the child handle is signaled.
        Ok(self.exited.as_ref().and_then(|(status, progress)| {
            (self.output.is_none() || progress.elapsed() >= EXIT_DRAIN).then(|| status.clone())
        }))
    }

    pub(crate) fn stop(&mut self) -> Option<ExitStatus> {
        if let Some(status) = self.child.try_wait().ok()? {
            return Some(status);
        }
        self.child.kill().ok()?;
        self.child.wait().ok()
    }

    pub(crate) fn complete_write(&mut self) -> io::Result<Option<usize>> {
        if self.writing {
            let result = self
                .completion
                .recv_timeout(FENCE_TIMEOUT)
                .map_err(|error| io::Error::new(ErrorKind::WouldBlock, error))?;
            self.writing = false;
            let count = result?;
            if count == 0 {
                return Err(ErrorKind::WriteZero.into());
            }
            return Ok(Some(count));
        }
        Ok(None)
    }
}

impl Read for TerminalPty {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.chunk.position() == self.chunk.get_ref().len() as u64 {
            let Some(output) = &self.output else {
                return Ok(0);
            };
            match output.try_recv() {
                Ok(result) => self.chunk = Cursor::new(result?),
                Err(TryRecvError::Empty) => return Err(ErrorKind::WouldBlock.into()),
                Err(TryRecvError::Disconnected) => {
                    self.output = None;
                    return Ok(0);
                }
            }
        }
        let count = self.chunk.read(buffer)?;
        if count > 0
            && let Some((_, progress)) = &mut self.exited
        {
            *progress = Instant::now();
        }
        Ok(count)
    }
}

impl Write for TerminalPty {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.writing {
            return match self.completion.try_recv() {
                Ok(result) => {
                    self.writing = false;
                    result
                }
                Err(TryRecvError::Empty) => Err(ErrorKind::WouldBlock.into()),
                Err(TryRecvError::Disconnected) => Err(ErrorKind::BrokenPipe.into()),
            };
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        let input = self.input.as_ref().ok_or(ErrorKind::BrokenPipe)?;
        input
            .try_send(bytes[..bytes.len().min(CHUNK)].to_vec())
            .map_err(|_| ErrorKind::BrokenPipe)?;
        self.writing = true;
        // The worker acknowledges progress only after the pipe accepted the bytes.
        Err(ErrorKind::WouldBlock.into())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.complete_write().map(|_| ())
    }
}

impl Drop for TerminalPty {
    fn drop(&mut self) {
        self.stop();
        drop(self.input.take());
        // The output thread drains until EOF while ClosePseudoConsole runs.
        drop(self.output.take());
    }
}

fn output_thread(
    mut reader: impl Read + Send + 'static,
) -> io::Result<Receiver<io::Result<Vec<u8>>>> {
    let (sender, output) = sync_channel(OUTPUT_CHUNKS);
    std::thread::Builder::new()
        .name("corvid-conpty-read".into())
        .spawn(move || {
            let mut bytes = [0; CHUNK];
            let mut connected = true;
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) if connected => {
                        connected = sender.send(Ok(bytes[..count].to_vec())).is_ok();
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == ErrorKind::BrokenPipe => break,
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            }
        })?;
    Ok(output)
}

type InputPipe = (SyncSender<Vec<u8>>, Receiver<io::Result<usize>>);

fn input_thread(mut writer: impl Write + Send + 'static) -> io::Result<InputPipe> {
    let (input, requests) = sync_channel::<Vec<u8>>(1);
    let (reply, completion) = sync_channel(1);
    std::thread::Builder::new()
        .name("corvid-conpty-write".into())
        .spawn(move || {
            while let Ok(bytes) = requests.recv() {
                let result = writer.write(&bytes);
                if reply.send(result).is_err() {
                    break;
                }
            }
        })?;
    Ok((input, completion))
}

fn size(viewport: Viewport) -> PtySize {
    PtySize {
        rows: viewport.rows.get(),
        cols: viewport.cols.get(),
        pixel_width: 0,
        pixel_height: 0,
    }
}
