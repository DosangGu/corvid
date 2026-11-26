mod backend;
mod commands;
mod composition;
mod history_view;
mod host;
mod input;
mod keys;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod locale;
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

pub(crate) use host::COMMAND as HOST_COMMAND;
pub(crate) use host::HostConfig;
pub use host::run_helper;
