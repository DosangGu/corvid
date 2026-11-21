use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, IsTerminal, Read, Write};
use std::os::unix::fs::OpenOptionsExt;

use anyhow::{Context, Result, ensure};
use compio::runtime::fd::PollFd as TtyFd;
use corvid_core::Viewport;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use nix::libc::{O_NOCTTY, O_NONBLOCK};
use nix::sys::termios::{FlushArg, tcflush};

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
        let (input, output) = (TtyFd::new(input)?, TtyFd::new(output)?);
        enable_raw_mode()?;
        let tty = Self { input, output };
        tty.discard_input()?;
        Ok(tty)
    }

    pub(crate) fn discard_input(&self) -> Result<()> {
        tcflush(self.input_file(), FlushArg::TCIFLUSH)?;
        Ok(())
    }

    pub(crate) fn try_read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        self.input_file().read(buffer)
    }

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
        &self.input
    }

    fn output_file(&self) -> &File {
        &self.output
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        let mut output = self.output_file();
        let _ = output.write_all(b"\x1b[?2004l\x1b[?1l\x1b>\x1b[0m\x1b[?25h\x1b[r\x1b[?1049l");
        let _ = disable_raw_mode();
    }
}
