pub mod connection;
pub mod desktop;
pub mod focus;
#[cfg(windows)]
pub mod launch;
#[cfg(windows)]
pub mod lifecycle;

pub const NAME: &str = "Corvid Agent Deck";
