use std::ffi::OsString;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use corvid_protocol::unix::RuntimeDir as StateDirectory;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use tokio::net::{
    UnixStream as Stream,
    unix::{OwnedReadHalf as ReadHalf, OwnedWriteHalf as WriteHalf},
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) type StateLock = nix::fcntl::Flock<std::fs::File>;

#[cfg(windows)]
pub(crate) use corvid_protocol::windows::StateDirectory;
#[cfg(windows)]
pub(crate) use tokio::net::windows::named_pipe::NamedPipeServer as Stream;
#[cfg(windows)]
pub(crate) type ReadHalf = tokio::io::ReadHalf<Stream>;
#[cfg(windows)]
pub(crate) type WriteHalf = tokio::io::WriteHalf<Stream>;
#[cfg(windows)]
pub(crate) type StateLock = std::fs::File;

pub(crate) fn split(stream: Stream) -> (ReadHalf, WriteHalf) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    return stream.into_split();
    #[cfg(windows)]
    tokio::io::split(stream)
}

pub(crate) fn decode_os_string(bytes: &[u8]) -> Option<OsString> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::ffi::OsStringExt;

        Some(OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(windows)]
    {
        std::str::from_utf8(bytes).ok().map(OsString::from)
    }
}
