#[cfg(target_os = "linux")]
#[path = "x11.rs"]
mod implementation;

#[cfg(windows)]
#[path = "windows.rs"]
mod implementation;

pub(super) use implementation::{configure, set_bounds, visible};

#[cfg(windows)]
pub(super) use implementation::start_move;

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod implementation;
