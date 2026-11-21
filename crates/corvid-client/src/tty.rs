use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, IsTerminal, Read, Write};
use std::os::unix::fs::OpenOptionsExt;

use anyhow::{Context, Result, ensure};
#[cfg(feature = "compio-tui")]
use compio::runtime::fd::PollFd as TtyFd;
use corvid_core::Viewport;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use nix::libc::{O_NOCTTY, O_NONBLOCK};
use nix::sys::termios::{FlushArg, tcflush};
#[cfg(not(feature = "compio-tui"))]
use tokio::io::{Interest, unix::AsyncFd as TtyFd};

pub(crate) struct Tty {
    input: TtyFd<File>,
    output: TtyFd<File>,
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
        Viewport::new(cols, rows).with_context(|| format!("terminal reports {cols}×{rows} cells"))
    }

    pub(crate) fn enter() -> Result<Self> {
        // Reopen the devices so nonblocking mode does not affect the inherited descriptors.
        #[cfg(target_os = "linux")]
        let (input_path, output_path) = ("/proc/self/fd/0".into(), "/proc/self/fd/1".into());
        #[cfg(target_os = "macos")]
        let (input_path, output_path) = (
            nix::unistd::ttyname(std::io::stdin())?,
            nix::unistd::ttyname(std::io::stdout())?,
        );
        let input_path: std::path::PathBuf = input_path;
        let output_path: std::path::PathBuf = output_path;
        let input = OpenOptions::new()
            .read(true)
            .custom_flags(O_NOCTTY | O_NONBLOCK)
            .open(input_path)?;
        let output = OpenOptions::new()
            .write(true)
            .custom_flags(O_NOCTTY | O_NONBLOCK)
            .open(output_path)?;
        #[cfg(feature = "compio-tui")]
        let (input, output) = (TtyFd::new(input)?, TtyFd::new(output)?);
        #[cfg(not(feature = "compio-tui"))]
        let input = register(input, Interest::READABLE)?;
        #[cfg(not(feature = "compio-tui"))]
        let output = register(output, Interest::WRITABLE)?;
        enable_raw_mode()?;
        let tty = Self { input, output };
        tty.discard_input()?;
        Ok(tty)
    }

    pub(crate) fn discard_input(&self) -> Result<()> {
        tcflush(self.input_file(), FlushArg::TCIFLUSH)?;
        Ok(())
    }

    #[cfg(not(feature = "compio-tui"))]
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

    pub(crate) fn try_read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        self.input_file().read(buffer)
    }

    #[cfg(not(feature = "compio-tui"))]
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

    #[cfg(feature = "compio-tui")]
    pub(crate) async fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.try_read(buffer) {
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    self.input.read_ready().await?;
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }

    #[cfg(feature = "compio-tui")]
    pub(crate) async fn write_all(&self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            match self.output_file().write(bytes) {
                Ok(0) => {
                    return Err(io::Error::new(
                        ErrorKind::WriteZero,
                        "terminal write stopped",
                    ));
                }
                Ok(written) => bytes = &bytes[written..],
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    self.output.write_ready().await?;
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn input_file(&self) -> &File {
        #[cfg(feature = "compio-tui")]
        return &self.input;
        #[cfg(not(feature = "compio-tui"))]
        self.input.get_ref()
    }

    fn output_file(&self) -> &File {
        #[cfg(feature = "compio-tui")]
        return &self.output;
        #[cfg(not(feature = "compio-tui"))]
        self.output.get_ref()
    }
}

#[cfg(not(feature = "compio-tui"))]
fn register(file: File, interest: Interest) -> io::Result<TtyFd<File>> {
    // The registration owns the File, whose descriptor remains stable and open.
    unsafe { TtyFd::register_with_interest(file, interest) }.map_err(Into::into)
}

impl Drop for Tty {
    fn drop(&mut self) {
        let mut output = self.output_file();
        let _ = output.write_all(b"\x1b[?2004l\x1b[?1l\x1b>\x1b[0m\x1b[?25h\x1b[r\x1b[?1049l");
        let _ = disable_raw_mode();
    }
}
