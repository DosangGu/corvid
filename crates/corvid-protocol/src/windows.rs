#[cfg(any(feature = "tokio", feature = "compio"))]
mod pipe;
pub mod security;
mod storage;

#[cfg(any(feature = "tokio", feature = "compio"))]
use std::path::Path;

#[cfg(any(feature = "tokio", feature = "compio"))]
use anyhow::Result;
#[cfg(feature = "tokio")]
use corvid_async::time::timeout;
#[cfg(feature = "tokio")]
use tokio::net::windows::named_pipe::NamedPipeClient;

#[cfg(any(feature = "tokio", feature = "compio"))]
use crate::COMMAND_TIMEOUT;

#[cfg(feature = "compio")]
pub use pipe::{CompioPipeListener, child_stdio_pipe, open_compio_pipe};
#[cfg(feature = "tokio")]
pub use pipe::{PipeListener, open_pipe, open_scoped};
pub use storage::{StateDirectory, check_component, require_local_acl_volume};

#[derive(Clone, Copy)]
pub enum DaemonPipe {
    Client,
    Control,
    Collector,
    Upgrade,
    TerminalHost,
}

impl DaemonPipe {
    #[cfg(any(feature = "tokio", feature = "compio"))]
    fn prefix(self) -> &'static str {
        match self {
            Self::Client => "corvid-daemon-v1-client",
            Self::Control => "corvid-daemon-v1-control",
            Self::Collector => "corvid-daemon-v1-collector",
            Self::Upgrade => "corvid-daemon-v1-upgrade",
            Self::TerminalHost => "corvid-terminal-host-v1",
        }
    }

    #[cfg(feature = "tokio")]
    pub fn bind(self, runtime: &StateDirectory) -> Result<PipeListener> {
        PipeListener::bind_scoped(self.prefix(), &runtime.file_id()?)
    }

    #[cfg(feature = "compio")]
    pub fn bind_compio(self, runtime: &StateDirectory) -> Result<CompioPipeListener> {
        CompioPipeListener::bind_scoped(self.prefix(), &runtime.file_id()?)
    }

    #[cfg(feature = "compio")]
    pub async fn connect_compio(
        self,
        path: &Path,
    ) -> Result<compio::fs::named_pipe::NamedPipeClient> {
        let runtime = StateDirectory::open(path)?;
        let instance = runtime.file_id()?;
        compio::time::timeout(COMMAND_TIMEOUT, pipe::open_compio(self.prefix(), &instance)).await?
    }

    #[cfg(feature = "tokio")]
    pub async fn connect(self, path: &Path) -> Result<NamedPipeClient> {
        let runtime = StateDirectory::open(path)?;
        let instance = runtime.file_id()?;
        timeout(COMMAND_TIMEOUT, pipe::open_scoped(self.prefix(), &instance)).await?
    }
}
