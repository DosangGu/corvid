use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, IsTerminal, Read, Write};
use std::os::unix::fs::OpenOptionsExt;

use anyhow::{Result, ensure};
use corvid_core::Viewport;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use nix::libc::{O_NOCTTY, O_NONBLOCK};
use nix::sys::termios::{FlushArg, tcflush};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

pub(crate) struct Tty {
    input: AsyncFd<File>,
    output: AsyncFd<File>,
}

impl Tty {
    pub(crate) fn check() -> Result<Viewport> {
        ensure!(
            io::stdin().is_terminal() && io::stdout().is_terminal(),
            "attach requires a TTY"
        );
        Self::viewport()
    }

    pub(crate) fn viewport() -> Result<Viewport> {
        let (cols, rows) = size()?;
        Ok(Viewport::new(cols, rows)?)
    }

    pub(crate) fn enter() -> Result<Self> {
        // Reopen the devices so nonblocking mode does not affect the inherited descriptors.
        let input = OpenOptions::new()
            .read(true)
            .custom_flags(O_NOCTTY | O_NONBLOCK)
            .open("/proc/self/fd/0")?;
        let output = OpenOptions::new()
            .write(true)
            .custom_flags(O_NOCTTY | O_NONBLOCK)
            .open("/proc/self/fd/1")?;
        let input = register(input, Interest::READABLE)?;
        let output = register(output, Interest::WRITABLE)?;
        enable_raw_mode()?;
        let tty = Self { input, output };
        tty.discard_input()?;
        Ok(tty)
    }

    pub(crate) fn discard_input(&self) -> Result<()> {
        tcflush(self.input.get_ref(), FlushArg::TCIFLUSH)?;
        Ok(())
    }

    pub(crate) async fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.input.readable().await?;
            match guard.try_io(|file| file.get_ref().read(buffer)) {
                Ok(Err(error)) if error.kind() == ErrorKind::Interrupted => {}
                Ok(result) => return result,
                Err(_) => {}
            }
        }
    }

    pub(crate) async fn write_all(&self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let mut guard = self.output.writable().await?;
            match guard.try_io(|file| file.get_ref().write(bytes)) {
                Ok(Ok(0)) => {
                    return Err(io::Error::new(
                        ErrorKind::WriteZero,
                        "terminal write stopped",
                    ));
                }
                Ok(Ok(written)) => bytes = &bytes[written..],
                Ok(Err(error)) if error.kind() == ErrorKind::Interrupted => {}
                Ok(Err(error)) => return Err(error),
                Err(_) => {}
            }
        }
        Ok(())
    }
}

fn register(file: File, interest: Interest) -> io::Result<AsyncFd<File>> {
    // The registration owns the File, whose descriptor remains stable and open.
    unsafe { AsyncFd::register_with_interest(file, interest) }.map_err(Into::into)
}

impl Drop for Tty {
    fn drop(&mut self) {
        let mut output = self.output.get_ref();
        let _ = output.write_all(b"\x1b[?2004l\x1b[?1l\x1b>\x1b[0m\x1b[?25h\x1b[?1049l");
        let _ = disable_raw_mode();
    }
}
