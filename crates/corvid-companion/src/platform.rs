#[cfg(windows)]
mod windows;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use corvid_protocol::unix::RuntimeDir as StateDirectory;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) type StateLock = nix::fcntl::Flock<std::fs::File>;

#[cfg(windows)]
pub(crate) use windows::{StateDirectory, TerminalRecord, TerminalRegistry, WezTermPane};
#[cfg(windows)]
pub(crate) type StateLock = std::fs::File;

#[cfg(windows)]
pub use windows::{WezTermCandidate, discover_wezterm_panes};
