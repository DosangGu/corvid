#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub(crate) use corvid_protocol::unix::RuntimeDir as StateDirectory;
#[cfg(target_os = "linux")]
pub(crate) type StateLock = nix::fcntl::Flock<std::fs::File>;

#[cfg(windows)]
pub(crate) use windows::{PipeListener, StateDirectory, open_pipe};
#[cfg(windows)]
pub(crate) type StateLock = std::fs::File;
