use corvid_protocol::wire::Envelope;
use corvid_protocol::{Frame, FrameError, FrameReader, FrameWriter, Session};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::connection::Connection;

pub(crate) struct Wire<R, W> {
    pub(crate) session: Session,
    pub(crate) reader: Reader<R>,
    pub(crate) writer: Writer<W>,
}

impl<R, W> From<Connection<R, W>> for Wire<R, W> {
    fn from(connection: Connection<R, W>) -> Self {
        Self {
            session: connection.session,
            reader: Reader::Tokio(connection.reader),
            writer: Writer::Tokio(connection.writer),
        }
    }
}

pub(crate) enum Reader<R> {
    Tokio(FrameReader<R>),
    #[cfg(feature = "compio-endpoints")]
    Compio(super::native::ReadQueue),
}

impl<R: AsyncRead + Unpin> Reader<R> {
    pub(crate) async fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        match self {
            Self::Tokio(reader) => reader.next_frame().await,
            #[cfg(feature = "compio-endpoints")]
            Self::Compio(reader) => reader.recv().await.transpose(),
        }
    }

    pub(crate) async fn next_envelope(&mut self) -> Result<Option<Envelope>, FrameError> {
        match self.next_frame().await? {
            Some(Frame::Structured(bytes)) => corvid_protocol::decode_envelope(&bytes).map(Some),
            Some(Frame::Render(_)) => Err(FrameError::WrongRole),
            None => Ok(None),
        }
    }
}

pub(crate) enum Writer<W> {
    Tokio(FrameWriter<W>),
    #[cfg(feature = "compio-endpoints")]
    Compio(super::native::WriteQueue),
}

impl<W: AsyncWrite + Unpin> Writer<W> {
    pub(crate) async fn envelope(&mut self, message: &Envelope) -> Result<(), FrameError> {
        match self {
            Self::Tokio(writer) => writer.envelope(message).await,
            #[cfg(feature = "compio-endpoints")]
            Self::Compio(writer) => super::native::send(writer, message.clone()).await,
        }
    }
}
