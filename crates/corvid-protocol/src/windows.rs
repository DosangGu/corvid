mod pipe;
pub mod security;
mod storage;

use std::path::Path;

use anyhow::Result;
use tokio::net::windows::named_pipe::NamedPipeClient;
use tokio::time::timeout;

use crate::COMMAND_TIMEOUT;

#[cfg(feature = "compio")]
pub use pipe::CompioPipeListener;
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
    fn prefix(self) -> &'static str {
        match self {
            Self::Client => "corvid-daemon-v1-client",
            Self::Control => "corvid-daemon-v1-control",
            Self::Collector => "corvid-daemon-v1-collector",
            Self::Upgrade => "corvid-daemon-v1-upgrade",
            Self::TerminalHost => "corvid-terminal-host-v1",
        }
    }

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

    pub async fn connect(self, path: &Path) -> Result<NamedPipeClient> {
        let runtime = StateDirectory::open(path)?;
        let instance = runtime.file_id()?;
        timeout(COMMAND_TIMEOUT, pipe::open_scoped(self.prefix(), &instance)).await?
    }
}
