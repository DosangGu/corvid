use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};

use anyhow::{Context, Result};
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, native_pty_system};

use crate::viewport::Viewport;

pub struct TerminalPty {
    master: Box<dyn MasterPty + Send>,
    io: File,
    child: Box<dyn Child + Send + Sync>,
}

impl TerminalPty {
    pub fn spawn(command: CommandBuilder, viewport: Viewport) -> Result<Self> {
        let pair = native_pty_system().openpty(viewport.into())?;
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
        self.master.resize(viewport.into())
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
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
        if matches!(self.child.try_wait(), Ok(None)) && self.child.kill().is_ok() {
            let _ = self.child.wait();
        }
    }
}
