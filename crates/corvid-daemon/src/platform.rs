use std::ffi::OsString;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use corvid_protocol::unix::RuntimeDir as StateDirectory;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) type StateLock = nix::fcntl::Flock<std::fs::File>;

#[cfg(windows)]
pub(crate) use corvid_protocol::windows::StateDirectory;
#[cfg(windows)]
pub(crate) type StateLock = std::fs::File;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use compio::net::UnixStream as Stream;
#[cfg(windows)]
pub(crate) type Stream = compio::runtime::fd::AsyncFd<std::os::windows::io::OwnedHandle>;
pub(crate) type ReadHalf = Stream;
pub(crate) type WriteHalf = Stream;

pub(crate) use corvid_protocol::CompioFrameReader as FrameReader;
pub(crate) use corvid_protocol::CompioFrameWriter as FrameWriter;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix_listener::Listener;
#[cfg(windows)]
pub(crate) use windows_listener::Listener;

pub(crate) fn split(stream: Stream) -> (ReadHalf, WriteHalf) {
    compio::io::util::Splittable::split(stream)
}

pub(crate) async fn render(
    writer: &mut FrameWriter<WriteHalf>,
    frame: corvid_protocol::RenderFrame,
) -> Result<(), corvid_protocol::FrameError> {
    return writer.render(frame).await;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix_listener {
    use anyhow::Result;
    pub(crate) use corvid_protocol::unix::CompioBoundSocket as Listener;
    use corvid_protocol::unix::{Instance, SocketRole};

    pub(crate) async fn bind<'a>(
        instance: &'a Instance<'_>,
        role: SocketRole,
    ) -> Result<Listener<'a>> {
        return instance.bind_compio(role).await;
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix_listener::bind;

#[cfg(windows)]
mod windows_listener {
    use anyhow::Result;
    use corvid_async::Mutex;
    use corvid_protocol::windows::CompioPipeListener as Pipe;
    use corvid_protocol::windows::{DaemonPipe, StateDirectory};

    pub(crate) struct Listener(Mutex<Pipe>);

    impl Listener {
        pub(crate) fn bind(runtime: &StateDirectory, role: DaemonPipe) -> Result<Self> {
            let pipe = role.bind_compio(runtime)?;
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
