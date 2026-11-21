#[cfg(feature = "compio")]
mod pipe;
pub mod security;
mod storage;

#[cfg(feature = "compio")]
use std::path::Path;

#[cfg(feature = "compio")]
use anyhow::Result;

#[cfg(feature = "compio")]
use crate::COMMAND_TIMEOUT;

#[cfg(feature = "compio")]
pub use pipe::{CompioPipeListener, child_stdio_pipe, open_compio_pipe, open_scoped};
#[cfg(feature = "compio")]
pub use pipe::{CompioPipeListener as PipeListener, open_compio_pipe as open_pipe};
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
    #[cfg(feature = "compio")]
    fn prefix(self) -> &'static str {
        match self {
            Self::Client => "corvid-daemon-v1-client",
            Self::Control => "corvid-daemon-v1-control",
            Self::Collector => "corvid-daemon-v1-collector",
            Self::Upgrade => "corvid-daemon-v1-upgrade",
            Self::TerminalHost => "corvid-terminal-host-v1",
        }
    }

    #[cfg(feature = "compio")]
    pub fn bind(self, runtime: &StateDirectory) -> Result<CompioPipeListener> {
        self.bind_compio(runtime)
    }

    #[cfg(feature = "compio")]
    pub async fn connect(self, path: &Path) -> Result<compio::fs::named_pipe::NamedPipeClient> {
        self.connect_compio(path).await
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
        compio::time::timeout(COMMAND_TIMEOUT, pipe::open_scoped(self.prefix(), &instance)).await?
    }
}
