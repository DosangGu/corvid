#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use corvid_protocol::unix::SocketRole;
#[cfg(windows)]
pub(crate) use corvid_protocol::windows::DaemonPipe as SocketRole;
