use std::io::{ErrorKind, Read};

use anyhow::Result;
use corvid_core::Viewport;
use portable_pty::{CommandBuilder, ExitStatus};

use super::input::{InputQueue, WriteFailure};
use super::{TerminalPty, TerminalScreen};

const LIVE_SCROLLBACK_ROWS: usize = 128;

pub(super) enum ReadState {
    Drained,
    Eof,
    BudgetUsed,
}

pub(super) struct TerminalSession {
    pty: TerminalPty,
    pub(super) screen: TerminalScreen,
    pub(super) input: InputQueue,
}

impl TerminalSession {
    pub(super) fn start(command: CommandBuilder, viewport: Viewport) -> Result<Self> {
        Ok(Self {
            pty: TerminalPty::spawn(command, viewport)?,
            screen: TerminalScreen::with_scrollback(viewport, LIVE_SCROLLBACK_ROWS),
            input: InputQueue::default(),
        })
    }

    pub(super) fn resize(&mut self, viewport: Viewport) -> Result<()> {
        self.pty.resize(viewport)?;
        self.screen.resize(viewport);
        Ok(())
    }

    pub(super) fn read_output(&mut self) -> Result<(ReadState, bool)> {
        let mut bytes = [0; 8192];
        let mut output = false;
        for _ in 0..16 {
            match self.pty.read(&mut bytes) {
                Ok(0) => return Ok((ReadState::Eof, output)),
                Ok(count) => {
                    self.input.reply(self.screen.process(&bytes[..count])?)?;
                    output = true;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    return Ok((ReadState::Drained, output));
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Ok((ReadState::BudgetUsed, output))
    }

    pub(super) fn flush_input(&mut self) -> Result<(), WriteFailure> {
        self.input.flush(&mut self.pty)
    }

    pub(super) fn fence_input(&mut self) -> Result<()> {
        self.complete_input_write()?;
        self.input.fence();
        Ok(())
    }

    pub(super) fn complete_input_write(&mut self) -> Result<()> {
        #[cfg(windows)]
        if let Some(written) = self.pty.complete_write()? {
            self.input.record_written(written);
        }
        Ok(())
    }

    pub(super) fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.pty.try_wait()
    }

    #[cfg(target_os = "linux")]
    pub(super) fn pid(&self) -> Option<u32> {
        self.pty.pid()
    }

    #[cfg(windows)]
    pub(super) fn process_key(&self) -> Option<crate::native_process::ProcessKey> {
        self.pty.process_key()
    }

    pub(super) fn stop(&mut self) -> Option<ExitStatus> {
        self.pty.stop()
    }
}
