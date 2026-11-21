#[cfg(all(
    feature = "compio",
    any(target_os = "linux", target_os = "macos", windows)
))]
mod compio_relay;
#[cfg(all(
    feature = "compio",
    any(target_os = "linux", target_os = "macos", windows)
))]
pub use compio_relay::{compio_relay, compio_relay_tui};

#[cfg(all(
    feature = "compio",
    any(target_os = "linux", target_os = "macos", windows)
))]
mod compio_connection;
#[cfg(all(
    feature = "compio",
    any(target_os = "linux", target_os = "macos", windows)
))]
pub use compio_connection::{compio_host, compio_ping};
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod attach;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod collector;
mod connection;
mod control;
mod endpoint;
#[cfg(not(feature = "compio-endpoints"))]
mod endpoint_time;
#[cfg(feature = "compio-endpoints")]
pub mod endpoint_time;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod keys;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod relay;
mod restore;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod runtime;
mod snapshot;
mod ssh_retry;
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
#[cfg(all(feature = "compio", any(target_os = "linux", target_os = "macos")))]
pub use collector::compio_register_shell_agent;
#[cfg(all(
    feature = "compio",
    any(target_os = "linux", target_os = "macos", windows)
))]
pub use collector::compio_submit_hook;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use collector::register_shell_agent;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub use collector::submit_hook;
pub use control::{ControllerRequest, ControllerScope};
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub use control::{agents, clients, controller, host, ping, status, workspaces};
#[cfg(feature = "compio-endpoints")]
pub use endpoint::CompioPresentation;
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
pub use ssh_retry::SshRetry;
#[cfg(windows)]
pub use upgrade::UpgradeTarget;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use upgrade::{UpgradeTarget, prepare_upgrade, prepare_upgrade_for};
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub use upgrade::{prepare_daemon_handoff, prepare_daemon_upgrade};
