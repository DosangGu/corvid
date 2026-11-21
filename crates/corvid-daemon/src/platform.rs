use std::ffi::OsString;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use corvid_protocol::unix::RuntimeDir as StateDirectory;
#[cfg(all(not(feature = "compio"), any(target_os = "linux", target_os = "macos")))]
pub(crate) use tokio::net::{
    UnixStream as Stream,
    unix::{OwnedReadHalf as ReadHalf, OwnedWriteHalf as WriteHalf},
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) type StateLock = nix::fcntl::Flock<std::fs::File>;

#[cfg(windows)]
pub(crate) use corvid_protocol::windows::StateDirectory;
#[cfg(all(not(feature = "compio"), windows))]
pub(crate) use tokio::net::windows::named_pipe::NamedPipeServer as Stream;
#[cfg(all(not(feature = "compio"), windows))]
pub(crate) type ReadHalf = tokio::io::ReadHalf<Stream>;
#[cfg(all(not(feature = "compio"), windows))]
pub(crate) type WriteHalf = tokio::io::WriteHalf<Stream>;
#[cfg(windows)]
pub(crate) type StateLock = std::fs::File;

#[cfg(all(feature = "compio", any(target_os = "linux", target_os = "macos")))]
pub(crate) use compio::net::UnixStream as Stream;
#[cfg(all(feature = "compio", windows))]
pub(crate) type Stream = compio::runtime::fd::AsyncFd<std::os::windows::io::OwnedHandle>;
#[cfg(feature = "compio")]
pub(crate) type ReadHalf = Stream;
#[cfg(feature = "compio")]
pub(crate) type WriteHalf = Stream;

#[cfg(feature = "compio")]
pub(crate) use corvid_protocol::CompioFrameReader as FrameReader;
#[cfg(feature = "compio")]
pub(crate) use corvid_protocol::CompioFrameWriter as FrameWriter;
#[cfg(not(feature = "compio"))]
pub(crate) use corvid_protocol::{FrameReader, FrameWriter};

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix_listener::Listener;
#[cfg(windows)]
pub(crate) use windows_listener::Listener;

pub(crate) fn split(stream: Stream) -> (ReadHalf, WriteHalf) {
    #[cfg(feature = "compio")]
    return compio::io::util::Splittable::split(stream);
    #[cfg(all(not(feature = "compio"), any(target_os = "linux", target_os = "macos")))]
    return stream.into_split();
    #[cfg(all(not(feature = "compio"), windows))]
    tokio::io::split(stream)
}

pub(crate) async fn render(
    writer: &mut FrameWriter<WriteHalf>,
    frame: corvid_protocol::RenderFrame,
) -> Result<(), corvid_protocol::FrameError> {
    #[cfg(feature = "compio")]
    return writer.render(frame).await;
    #[cfg(not(feature = "compio"))]
    writer.render(&frame).await
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix_listener {
    use anyhow::Result;
    #[cfg(not(feature = "compio"))]
    pub(crate) use corvid_protocol::unix::BoundSocket as Listener;
    #[cfg(feature = "compio")]
    pub(crate) use corvid_protocol::unix::CompioBoundSocket as Listener;
    use corvid_protocol::unix::{Instance, SocketRole};

    pub(crate) async fn bind<'a>(
        instance: &'a Instance<'_>,
        role: SocketRole,
    ) -> Result<Listener<'a>> {
        #[cfg(feature = "compio")]
        return instance.bind_compio(role).await;
        #[cfg(not(feature = "compio"))]
        instance.bind(role).await
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix_listener::bind;

#[cfg(windows)]
mod windows_listener {
    use anyhow::Result;
    use corvid_async::Mutex;
    #[cfg(feature = "compio")]
    use corvid_protocol::windows::CompioPipeListener as Pipe;
    #[cfg(not(feature = "compio"))]
    use corvid_protocol::windows::PipeListener as Pipe;
    use corvid_protocol::windows::{DaemonPipe, StateDirectory};

    pub(crate) struct Listener(Mutex<Pipe>);

    impl Listener {
        pub(crate) fn bind(runtime: &StateDirectory, role: DaemonPipe) -> Result<Self> {
            #[cfg(feature = "compio")]
            let pipe = role.bind_compio(runtime)?;
            #[cfg(not(feature = "compio"))]
            let pipe = role.bind(runtime)?;
            Ok(Self(Mutex::new(pipe)))
        }

        pub(crate) async fn accept(&self) -> Result<super::Stream> {
            self.0.lock().await.accept().await
        }
    }
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
