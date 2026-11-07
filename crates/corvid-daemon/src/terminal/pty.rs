use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};

use anyhow::{Context, Result};
use corvid_core::Viewport;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};

fn pty_size(viewport: Viewport) -> PtySize {
    PtySize {
        rows: viewport.rows.get(),
        cols: viewport.cols.get(),
        pixel_width: 0,
        pixel_height: 0,
    }
}

pub struct TerminalPty {
    master: Box<dyn MasterPty + Send>,
    io: File,
    child: Box<dyn Child + Send + Sync>,
}

impl TerminalPty {
    pub fn spawn(command: CommandBuilder, viewport: Viewport) -> Result<Self> {
        let pair = native_pty_system().openpty(pty_size(viewport))?;
        let raw_fd = pair
            .master
            .as_raw_fd()
            .context("missing Unix PTY descriptor")?;

        // The master owns this descriptor throughout the duplication.
        let fd = unsafe { BorrowedFd::borrow_raw(raw_fd) };
        let io = File::from(fd.try_clone_to_owned()?);
        let flags = OFlag::from_bits_retain(fcntl(&io, FcntlArg::F_GETFL)?);
        fcntl(&io, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;

        let child = pair.slave.spawn_command(command)?;
        Ok(Self {
            master: pair.master,
            io,
            child,
        })
    }

    pub fn resize(&self, viewport: Viewport) -> Result<()> {
        self.master.resize(pty_size(viewport))
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub(crate) fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    pub(crate) fn stop(&mut self) -> Option<ExitStatus> {
        if let Some(status) = self.child.try_wait().ok()? {
            return Some(status);
        }
        self.child.kill().ok()?;
        self.child.wait().ok()
    }
}

impl AsFd for TerminalPty {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.io.as_fd()
    }
}

impl Read for TerminalPty {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.io.read(buf) {
            // Linux returns EIO when the last slave descriptor closes.
            Err(error) if error.raw_os_error() == Some(nix::libc::EIO) => Ok(0),
            result => result,
        }
    }
}

impl Write for TerminalPty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

impl Drop for TerminalPty {
    fn drop(&mut self) {
        self.stop();
    }
}
