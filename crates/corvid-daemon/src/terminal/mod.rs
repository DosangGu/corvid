mod backend;
mod host;
mod input;
mod keys;
mod paste;
#[cfg(target_os = "linux")]
mod pty;
#[cfg(windows)]
#[path = "pty/windows.rs"]
mod pty;
mod screen;
mod session;
mod status;
mod tabs;
mod worker;

pub(crate) use keys::{InputMode, Keymap, TabCommand};
pub use pty::TerminalPty;
pub use screen::TerminalScreen;
pub(crate) use status::TuiViewport;
pub(crate) use worker::{
    Action, AttachSpec, Event, Launch, LaunchScope, Render, RetainedTerminal, TerminalHandle,
};

pub(crate) use host::HostConfig;
pub use host::run_helper;
pub(crate) const PROMOTED_RENDER_GENERATION: u64 = 1 << 63;
