mod frame;
mod preflight;
mod provider;
mod session;
pub mod snapshot;
#[cfg(any(target_os = "linux", windows))]
pub mod upgrade;

#[cfg(target_os = "linux")]
pub mod unix;
#[cfg(windows)]
pub mod windows;

pub mod wire {
    include!(concat!(env!("OUT_DIR"), "/corvid.protocol.v1.rs"));
}

pub use frame::{
    CONTROL_LIMIT, FRAME_PREAMBLE, Frame, FrameError, FrameReader, FrameRole, FrameWriter,
    RENDER_LIMIT, RenderFrame, decode_envelope,
};
pub use session::{
    CAPABILITY_QUERY, COMMAND_TIMEOUT, PROTOCOL_MAJOR, PROTOCOL_MINOR, PeerRole, ProtocolFault,
    Session, accept_hello, accept_hello_with_profiles, check_hello_result, hello,
};
pub mod collector;
pub mod controller;

pub const CLAIM_WORKSPACE_CAPABILITY: &str = "claim_workspace.v1";
pub const CREATE_VIEW_CAPABILITY: &str = "create_view.v1";
pub const CLOSE_VIEW_CAPABILITY: &str = "close_view.v1";
pub const PANE_PREVIEW_CAPABILITY: &str = "pane_preview.v1";
pub const TERMINAL_HISTORY_CAPABILITY: &str = "terminal_history.v1";
pub const PROCESS_AGENT_LAUNCH_CAPABILITY: &str = "agent_launch.process.v1";
pub const OBSERVER_CAPABILITY: &str = "attach.observer.v1";
pub const WRITER_HANDOFF_CAPABILITY: &str = "writer_handoff.v1";
pub const WRITER_HANDOFF_REQUEST_CAPABILITY: &str = "writer_handoff.request.v1";
pub const PANE_WRITERS_CAPABILITY: &str = "pane_writers.v1";
pub const SESSION_CLIENTS_CAPABILITY: &str = "session_clients.v1";
pub const TERMINAL_HISTORY_MAX_ROWS: usize = 2048;
pub const TERMINAL_HISTORY_MAX_BYTES: usize = 192 * 1024;
pub const MOVE_TAB_CAPABILITY: &str = "move_tab.v1";
pub const STATUS_BAR_CAPABILITY: &str = "status_bar.v1";
pub const ADAPTIVE_VIEWPORT_CAPABILITY: &str = "adaptive_viewport.v1";
pub const RESPONSE_NOTIFICATION_CAPABILITY: &str = "response_notification.v1";
pub const DEFAULT_SHELL_CAPABILITY: &str = "default_shell.v1";
pub const LEGACY_DEFAULT_LAUNCH_CAPABILITY: &str = "default_launch.v1";
pub const DAEMON_UPGRADE_CAPABILITY: &str = "daemon_upgrade.v1";
pub const DAEMON_HANDOFF_CAPABILITY: &str = "daemon_handoff.v1";
