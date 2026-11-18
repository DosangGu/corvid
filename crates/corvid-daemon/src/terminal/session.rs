use std::io::{ErrorKind, Read};

use anyhow::Result;
use corvid_core::Viewport;
use corvid_protocol::wire::TerminalHistory;
use portable_pty::{CommandBuilder, ExitStatus};

use super::input::{InputQueue, WriteFailure};
use super::{TerminalPty, TerminalScreen};

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
            screen: TerminalScreen::live(viewport),
            input: InputQueue::default(),
        })
    }

    pub(super) fn history(&mut self, shell_only: bool) -> TerminalHistory {
        if !shell_only {
            return self.screen.history();
        }
        if !self.screen.allows_shell_history()
            || !self
                .pty
                .foreground_shell(self.screen.shell_prompt() == Some(true))
        {
            return TerminalHistory {
                columns: self.screen.viewport().cols.get().into(),
                ..Default::default()
            };
        }
        let mut history = self.screen.history();
        history.shell_foreground = true;
        history
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for(session: &mut TerminalSession, ready: impl Fn(&mut TerminalSession) -> bool) {
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            session.read_output().unwrap();
            assert!(session.flush_input().is_ok());
            if ready(session) {
                return;
            }
            assert!(
                Instant::now() < until,
                "PowerShell did not reach the expected shell phase: {:?}",
                session.screen.shell_prompt()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn wheel_history_stays_live_while_powershell_runs_a_console_program() {
        let mut command = CommandBuilder::new("powershell.exe");
        command.args(["-NoLogo", "-NoProfile"]);
        let mut session = TerminalSession::start(command, Viewport::new(80, 24).unwrap()).unwrap();
        wait_for(&mut session, |session| {
            session.screen.snapshot().contents().contains("PS ")
                && session.history(true).shell_foreground
        });

        assert!(
            session
                .input
                .user(b"cmd.exe /d /q /c 'echo CHILD^_READY&set /p answer='\r".to_vec())
        );
        wait_for(&mut session, |session| {
            session
                .screen
                .snapshot()
                .contents()
                .lines()
                .any(|line| line.trim() == "CHILD_READY")
        });
        assert!(!session.history(true).shell_foreground);
        assert!(!session.history(false).lines.is_empty());
        assert!(session.input.user(b"done\r".to_vec()));

        wait_for(&mut session, |session| {
            session.history(true).shell_foreground
        });
    }
}
