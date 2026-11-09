mod frame;
mod preflight;
mod provider;
mod session;
pub mod snapshot;

#[cfg(target_os = "linux")]
pub mod unix;

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
pub const MOVE_TAB_CAPABILITY: &str = "move_tab.v1";
pub const ADAPTIVE_VIEWPORT_CAPABILITY: &str = "adaptive_viewport.v1";
pub const RESPONSE_NOTIFICATION_CAPABILITY: &str = "response_notification.v1";
pub const DEFAULT_LAUNCH_CAPABILITY: &str = "default_launch.v1";
