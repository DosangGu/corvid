use std::path::Path;

use anyhow::{Result, ensure};
use corvid_core::Viewport;
use corvid_protocol::wire::TerminalHistory;
use portable_pty::{CommandBuilder, ExitStatus};
use uuid::Uuid;
use vt100::Screen;

use super::host::{Client, HostConfig, Operation};
use super::input::WriteFailure;
use super::session::{ReadState, TerminalSession};

pub(super) enum Backend {
    Local(Box<TerminalSession>),
    Hosted(Client),
}

pub(super) struct Tick {
    pub(super) output: bool,
    pub(super) exit: Option<ExitStatus>,
    pub(super) failure: Option<WriteFailure>,
}

impl Backend {
    pub(super) fn start(
        host: Option<&HostConfig>,
        id: Uuid,
        command: CommandBuilder,
        viewport: Viewport,
    ) -> Result<Self> {
        if let Some(host) = host {
            return Ok(Self::Hosted(Client::start(host, id, command, viewport)?));
        }
        Ok(Self::Local(Box::new(TerminalSession::start(
            command, viewport,
        )?)))
    }

    pub(super) fn recover(root: &Path, id: Uuid) -> Result<Self> {
        Ok(Self::Hosted(Client::connect(root, id, false)?))
    }

    pub(super) fn poll(&mut self) -> Result<Tick> {
        match self {
            Self::Local(session) => {
                let (state, output) = session.read_output()?;
                let exit = if matches!(state, ReadState::BudgetUsed) {
                    None
                } else {
                    session.try_wait()?
                };
                let eof = matches!(state, ReadState::Eof);
                let failure = if exit.is_some() {
                    session.input.interrupted_paste()
                } else if eof {
                    None
                } else {
                    session.flush_input().err()
                };
                Ok(Tick {
                    output,
                    exit,
                    failure,
                })
            }
            Self::Hosted(client) => {
                let response = client.command(Operation::Poll, Vec::new())?;
                ensure!(!response.rejected, "terminal host poll failed");
                let exit = response.status();
                Ok(Tick {
                    output: response.changed,
                    exit,
                    failure: response.write_failure(),
                })
            }
        }
    }

    pub(super) fn paste_pending(&self) -> bool {
        match self {
            Self::Local(session) => session.input.paste_pending(),
            Self::Hosted(client) => client.paste_pending,
        }
    }

    pub(super) fn keys(&mut self, bytes: Vec<u8>) -> bool {
        match self {
            Self::Local(session) => session.input.user(bytes),
            Self::Hosted(client) => client
                .command(Operation::Keys, bytes)
                .is_ok_and(|reply| !reply.rejected),
        }
    }

    pub(super) fn paste(&mut self, bytes: Vec<u8>) -> bool {
        match self {
            Self::Local(session) => session.input.paste(bytes, session.screen.bracketed_paste()),
            Self::Hosted(client) => client
                .command(Operation::Paste, bytes)
                .is_ok_and(|reply| !reply.rejected),
        }
    }

    pub(super) fn fence(&mut self) -> Result<()> {
        match self {
            Self::Local(session) => session.fence_input(),
            Self::Hosted(client) => {
                ensure!(
                    !client.command(Operation::Fence, Vec::new())?.rejected,
                    "terminal input could not be fenced"
                );
                Ok(())
            }
        }
    }

    pub(super) fn resize(&mut self, viewport: Viewport) -> Result<()> {
        match self {
            Self::Local(session) => session.resize(viewport),
            Self::Hosted(client) => client.resize(viewport),
        }
    }

    pub(super) fn resize_active(&mut self, viewport: Viewport) -> Result<()> {
        if let Self::Local(session) = self {
            session.complete_input_write()?;
            session.resize(viewport)?;
            session.input.fence();
            return Ok(());
        }
        self.resize(viewport)
    }

    pub(super) fn preview(&mut self) -> Result<(Viewport, Vec<u8>)> {
        match self {
            Self::Local(session) => {
                let viewport = session.screen.viewport();
                Ok((viewport, session.screen.redraw(viewport)?.into_bytes()))
            }
            Self::Hosted(client) => client.preview(),
        }
    }

    pub(super) fn history(&mut self) -> Result<TerminalHistory> {
        match self {
            Self::Local(session) => Ok(session.screen.history()),
            Self::Hosted(client) => client.history(),
        }
    }

    pub(super) fn render(
        &mut self,
        previous: Option<&Option<Box<Screen>>>,
        outer: Viewport,
        footer: Option<(u16, &str)>,
    ) -> Result<(String, Option<Box<Screen>>)> {
        match self {
            Self::Local(session) => {
                let previous = previous.and_then(Option::as_deref);
                let ansi = session.screen.render(previous, outer, footer)?;
                Ok((ansi, Some(Box::new(session.screen.snapshot()))))
            }
            Self::Hosted(client) => Ok((client.render(previous.is_some(), outer, footer)?, None)),
        }
    }

    pub(super) fn retain(&mut self, retain: bool) -> Result<bool> {
        let Self::Hosted(client) = self else {
            return Ok(false);
        };
        let operation = if retain {
            Operation::Retain
        } else {
            Operation::CancelRetain
        };
        let response = client.command(operation, Vec::new())?;
        ensure!(
            !response.rejected,
            "terminal host could not prepare handoff"
        );
        Ok(response.exit_code.is_none())
    }

    pub(super) fn is_retained(&self) -> bool {
        matches!(self, Self::Hosted(client) if client.retained)
    }

    #[cfg(target_os = "linux")]
    pub(super) fn pid(&self) -> Option<u32> {
        match self {
            Self::Local(session) => session.pid(),
            Self::Hosted(client) => client.pid,
        }
    }

    pub(super) fn status(&mut self) -> Result<Option<ExitStatus>> {
        match self {
            Self::Local(session) => Ok(session.try_wait()?),
            Self::Hosted(client) => Ok(client.command(Operation::Poll, Vec::new())?.status()),
        }
    }

    #[cfg(windows)]
    pub(super) fn process_key(&self) -> Option<crate::native_process::ProcessKey> {
        match self {
            Self::Local(session) => session.process_key(),
            Self::Hosted(client) => client.process_key,
        }
    }

    pub(super) fn stop(&mut self) -> Option<ExitStatus> {
        match self {
            Self::Local(session) => session.stop(),
            Self::Hosted(client) => client.command(Operation::Stop, Vec::new()).ok()?.status(),
        }
    }
}
