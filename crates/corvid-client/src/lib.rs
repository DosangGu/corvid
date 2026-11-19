#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod attach;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod collector;
mod connection;
mod control;
mod endpoint;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod keys;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod relay;
mod restore;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod runtime;
mod snapshot;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod tty;
#[cfg(windows)]
#[path = "tty/windows.rs"]
mod tty;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod upgrade;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub mod view_state;

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub use attach::{AttachOptions, attach, check_tty};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use collector::register_shell_agent;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub use collector::submit_hook;
pub use control::{ControllerRequest, ControllerScope};
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub use control::{agents, clients, controller, host, ping, status, workspaces};
#[cfg(windows)]
pub use endpoint::check_system_openssh;
pub use endpoint::{
    ControlEndpoint, DaemonInstallation, HostPlatform, InteractiveConnection, RemoteHost, SshAlias,
    WslDistribution,
};
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub use relay::{relay, relay_tui};
pub use restore::AttachRecovery;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub use runtime::{daemon_missing, runtime_path};
pub use snapshot::SnapshotWatch;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub use snapshot::{snapshot, watch_snapshot};
#[cfg(windows)]
pub use upgrade::UpgradeTarget;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use upgrade::{UpgradeTarget, prepare_upgrade, prepare_upgrade_for};
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub use upgrade::{prepare_daemon_handoff, prepare_daemon_upgrade};
