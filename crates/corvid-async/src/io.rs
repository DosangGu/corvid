//! Borrowed-buffer interoperability for bounded control streams and fixtures.

use std::pin::Pin;

use compio_io::{compat::AsyncStream, util::Splittable};
pub type Native<S> = Pin<Box<AsyncStream<S>>>;
pub use futures_util::io::{
    AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf,
};

pub fn adapt<S: Splittable>(stream: S) -> Native<S> {
    Box::pin(AsyncStream::new(stream))
}

pub fn split<R: AsyncRead + AsyncWrite>(stream: R) -> (ReadHalf<R>, WriteHalf<R>) {
    stream.split()
}

pub fn split_native<S: Splittable + 'static>(
    stream: S,
) -> (ReadHalf<Native<S>>, WriteHalf<Native<S>>)
where
    S::ReadHalf: compio::io::AsyncRead + Unpin + 'static,
    S::WriteHalf: compio::io::AsyncWrite + Unpin + 'static,
{
    split(adapt(stream))
}

/// Loopback fixture peer streams with independent read and write halves.
pub fn fixture_pair() -> std::io::Result<(
    async_io::Async<std::net::TcpStream>,
    async_io::Async<std::net::TcpStream>,
)> {
    use std::net::{Ipv4Addr, TcpListener, TcpStream};

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let client = TcpStream::connect(listener.local_addr()?)?;
    let (server, _) = listener.accept()?;
    Ok((async_io::Async::new(client)?, async_io::Async::new(server)?))
}
