pub mod connection;
pub mod desktop;
#[cfg(windows)]
pub mod launch;
#[cfg(windows)]
pub mod lifecycle;

pub const NAME: &str = "Corvid Agent Deck";
