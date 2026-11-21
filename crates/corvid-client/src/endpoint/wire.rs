use corvid_protocol::wire::Envelope;
use corvid_protocol::{Frame, FrameError, Session};

use super::native::{self, ReadQueue, WriteQueue};

pub(crate) struct Wire {
    pub(crate) session: Session,
    pub(crate) reader: Reader,
    pub(crate) writer: Writer,
}

impl Wire {
    pub(crate) fn from_native((session, reader, writer): (Session, ReadQueue, WriteQueue)) -> Self {
        Self {
            session,
            reader: Reader(reader),
            writer: Writer(writer),
        }
    }
}

pub(crate) struct Reader(ReadQueue);

impl Reader {
    pub(crate) async fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        self.0.recv().await.transpose()
    }

    pub(crate) async fn next_envelope(&mut self) -> Result<Option<Envelope>, FrameError> {
        match self.next_frame().await? {
            Some(Frame::Structured(bytes)) => corvid_protocol::decode_envelope(&bytes).map(Some),
            Some(Frame::Render(_)) => Err(FrameError::WrongRole),
            None => Ok(None),
        }
    }
}

pub(crate) struct Writer(WriteQueue);

impl Writer {
    pub(crate) async fn envelope(&mut self, message: &Envelope) -> Result<(), FrameError> {
        native::send(&self.0, message.clone()).await
    }
}
