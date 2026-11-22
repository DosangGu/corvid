mod backend;
mod commands;
mod composition;
mod host;
mod input;
mod keys;
mod navigation;
mod paste;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod pty;
#[cfg(windows)]
#[path = "pty/windows.rs"]
mod pty;
mod rename;
mod screen;
mod session;
mod status;
mod tabs;
mod title_motion;
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
