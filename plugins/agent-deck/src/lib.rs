pub mod connection;
pub mod desktop;
#[cfg(windows)]
pub mod launch;
#[cfg(windows)]
pub mod lifecycle;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod unix_lifecycle;

pub const NAME: &str = "Corvid Agent Deck";
