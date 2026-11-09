#[cfg(target_os = "linux")]
mod attach;
#[cfg(target_os = "linux")]
mod collector;
mod connection;
mod control;
mod endpoint;
#[cfg(target_os = "linux")]
mod keys;
#[cfg(target_os = "linux")]
mod relay;
mod snapshot;
#[cfg(target_os = "linux")]
mod tty;

#[cfg(target_os = "linux")]
pub use attach::{AttachOptions, attach, check_tty};
#[cfg(target_os = "linux")]
pub use collector::submit_hook;
pub use control::{ControllerRequest, ControllerScope};
#[cfg(target_os = "linux")]
pub use control::{agents, clients, controller, host, ping, status, workspaces};
#[cfg(windows)]
pub use endpoint::check_system_openssh;
pub use endpoint::{ControlEndpoint, SshAlias};
#[cfg(target_os = "linux")]
pub use relay::relay;
pub use snapshot::SnapshotWatch;
#[cfg(target_os = "linux")]
pub use snapshot::{snapshot, watch_snapshot};
