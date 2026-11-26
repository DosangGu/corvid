#[cfg(feature = "ansi-screen")]
pub mod ansi_screen;

#[cfg(feature = "compio")]
pub use frame::compio::{CompioFrameReader, CompioFrameWriter};

mod frame;
pub mod history;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub mod paths;
mod preflight;
pub mod program_status;
mod provider;
mod session;
pub mod snapshot;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub mod upgrade;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod unix;
#[cfg(windows)]
pub mod windows;

pub mod wire {
    include!(concat!(env!("OUT_DIR"), "/corvid.protocol.v1.rs"));
}

impl wire::PromptObservation {
    pub fn confirms_execution(self) -> bool {
        matches!(
            self,
            Self::ModelRequested | Self::ModelAttemptFinished | Self::ToolExecuted
        )
    }
}

pub use frame::{
    CONTROL_LIMIT, FRAME_PREAMBLE, Frame, FrameError, FrameRole, RENDER_LIMIT, RenderFrame,
    decode_envelope,
};
#[cfg(feature = "borrowed-io")]
pub use frame::{FrameReader, FrameWriter};
pub use session::{
    CAPABILITY_QUERY, COMMAND_TIMEOUT, PROTOCOL_MAJOR, PROTOCOL_MINOR, PeerRole, ProtocolFault,
    Session, accept_hello, accept_hello_with_profiles, check_hello_result, hello,
};
pub mod collector;
pub mod controller;

pub const CLAIM_WORKSPACE_CAPABILITY: &str = "claim_workspace.v1";
pub const CREATE_VIEW_CAPABILITY: &str = "create_view.v1";
pub const WORKSPACE_DIRECTORY_CAPABILITY: &str = "workspace.directory.v1";
pub const VIEW_TITLES_CAPABILITY: &str = "view_titles.v1";
pub const CLOSE_VIEW_CAPABILITY: &str = "close_view.v1";
pub const PANE_PREVIEW_CAPABILITY: &str = "pane_preview.v1";
pub const TAB_PREVIEW_CAPABILITY: &str = "tab_preview.v1";
pub const TERMINAL_HISTORY_CAPABILITY: &str = "terminal_history.v1";
pub const COMMAND_HISTORY_CAPABILITY: &str = "command_history.v1";
pub const PROCESS_AGENT_LAUNCH_CAPABILITY: &str = "agent_launch.process.v1";
pub const PANE_WRITERS_CAPABILITY: &str = "pane_writers.v1";
pub const SESSION_CLIENTS_CAPABILITY: &str = "session_clients.v1";
pub const WRITER_RESUME_CAPABILITY: &str = "writer_resume.v1";
pub const ATTACH_AGENT_CAPABILITY: &str = "attach_agent.v1";
pub const PARK_TERMINAL_CAPABILITY: &str = "park_terminal.v1";
pub const GRACEFUL_DETACH_CAPABILITY: &str = "graceful_detach.v1";
/// User-initiated attaches may resume a writer, including default-shell attaches.
pub const RESUME_ATTACH_CAPABILITY: &str = "attach.resume.v1";
/// Explicitly restore saved provider conversations through the interactive socket.
pub const RESTORE_AGENTS_CAPABILITY: &str = "workspace.restore_agents.v1";
/// Opt in to a single-pane WorkspaceRestored reply before attaching to a recoverable empty pane.
pub const ATTACH_RECOVERY_CAPABILITY: &str = "attach.recovery.v1";
pub const CLIPBOARD_WRITE_CAPABILITY: &str = "clipboard_write.v1";
pub const CLIPBOARD_WRITE_MAX_BYTES: usize = 64 * 1024;

pub fn valid_clipboard_text(text: &str) -> bool {
    !text.is_empty() && text.len() <= CLIPBOARD_WRITE_MAX_BYTES && !text.contains('\0')
}

pub const TERMINAL_HISTORY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(180);
pub const TERMINAL_HISTORY_MAX_ROWS: usize = 2048;
pub const TERMINAL_HISTORY_MAX_BYTES: usize = 192 * 1024;
pub const TERMINAL_HISTORY_MAX_COMMANDS: usize = 256;
pub const MOVE_TAB_CAPABILITY: &str = "move_tab.v1";
pub const REORDER_TAB_CAPABILITY: &str = "reorder_tab.v1";
pub const WORKSPACE_UI_CAPABILITY: &str = "workspace_ui.v1";
pub const STATUS_BAR_CAPABILITY: &str = "status_bar.v1";
pub const ADAPTIVE_VIEWPORT_CAPABILITY: &str = "adaptive_viewport.v1";
pub const RESPONSE_NOTIFICATION_CAPABILITY: &str = "response_notification.v1";
pub const DEFAULT_SHELL_CAPABILITY: &str = "default_shell.v1";
pub const LEGACY_DEFAULT_LAUNCH_CAPABILITY: &str = "default_launch.v1";
pub const DAEMON_UPGRADE_CAPABILITY: &str = "daemon_upgrade.v1";
pub const DAEMON_HANDOFF_CAPABILITY: &str = "daemon_handoff.v1";

pub const MAX_HYPERLINK_URI_BYTES: usize = 2048;
pub const MAX_HYPERLINK_ID_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hyperlink {
    pub uri: String,
    pub id: Option<String>,
}

pub fn sanitize_hyperlink(uri: &str, id: Option<&str>) -> Option<Hyperlink> {
    if uri.is_empty() || uri.len() > MAX_HYPERLINK_URI_BYTES || uri.chars().any(is_unsafe_char) {
        return None;
    }
    let id = match id.filter(|id| !id.is_empty()) {
        Some(id) => {
            if id.len() > MAX_HYPERLINK_ID_BYTES
                || id
                    .chars()
                    .any(|ch| is_unsafe_char(ch) || ch == ';' || ch == ':')
            {
                return None;
            }
            Some(id.to_owned())
        }
        None => None,
    };
    Some(Hyperlink {
        uri: uri.to_owned(),
        id,
    })
}

fn is_unsafe_char(ch: char) -> bool {
    ch.is_control()
        || matches!(
            ch,
            '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}
