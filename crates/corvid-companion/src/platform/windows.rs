mod wezterm;
mod wezterm_registry;

pub(crate) use corvid_protocol::windows::{PipeListener, StateDirectory, open_pipe};
pub(crate) use wezterm::WezTermPane;
pub use wezterm::{WezTermCandidate, discover_wezterm_panes};
pub(crate) use wezterm_registry::{TerminalRecord, TerminalRegistry};
