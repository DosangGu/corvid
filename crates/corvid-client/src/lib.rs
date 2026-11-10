#[cfg(any(target_os = "linux", windows))]
mod attach;
#[cfg(target_os = "linux")]
mod collector;
mod connection;
mod control;
mod endpoint;
#[cfg(any(target_os = "linux", windows))]
mod keys;
#[cfg(target_os = "linux")]
mod relay;
mod snapshot;
#[cfg(target_os = "linux")]
mod tty;
#[cfg(windows)]
#[path = "tty/windows.rs"]
mod tty;
#[cfg(target_os = "linux")]
mod upgrade;

#[cfg(any(target_os = "linux", windows))]
pub use attach::{AttachOptions, attach, check_tty};
#[cfg(target_os = "linux")]
pub use collector::{register_shell_agent, submit_hook};
pub use control::{ControllerRequest, ControllerScope};
#[cfg(any(target_os = "linux", windows))]
pub use control::{agents, clients, controller, host, ping, status, workspaces};
#[cfg(windows)]
pub use endpoint::check_system_openssh;
pub use endpoint::{ControlEndpoint, InteractiveConnection, SshAlias, SshDaemon};
#[cfg(target_os = "linux")]
pub use relay::{relay, relay_tui};
pub use snapshot::SnapshotWatch;
#[cfg(any(target_os = "linux", windows))]
pub use snapshot::{snapshot, watch_snapshot};
#[cfg(target_os = "linux")]
pub use upgrade::{UpgradeTarget, prepare_upgrade};
