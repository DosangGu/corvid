use std::io::ErrorKind;

use ::compio::BufResult;
use ::compio::buf::{IntoInner, IoBuf, IoBufMut};
use ::compio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use prost::Message;

use super::{
    COMMAND_TIMEOUT, FRAME_PREAMBLE, Frame, FrameError, FrameRole, RENDER, RENDER_HEADER_LIMIT,
    RenderFrame, STRUCTURED, as_envelope, check_render_header, decode_render_header,
    frame_body_length, frame_length, frame_prefix, render_header_length,
};
use crate::wire::Envelope;

// Batch small frames while retaining a direct path for the remainder of
// large payloads.
const READ_AHEAD: usize = 512;

/// A cancelled or failed frame read leaves the stream unusable.
pub struct CompioFrameReader<R> {
    reader: R,
    role: FrameRole,
    buffered: Vec<u8>,
    offset: usize,
}

impl<R: AsyncRead> CompioFrameReader<R> {
    pub async fn start(mut reader: R, role: FrameRole) -> Result<Self, FrameError> {
        if read_exact(&mut reader, [0; 4]).await? != FRAME_PREAMBLE {
            return Err(FrameError::ProtocolMismatch);
        }
        Ok(Self {
            reader,
            role,
            buffered: Vec::with_capacity(READ_AHEAD),
            offset: 0,
        })
    }

    pub async fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        if self.offset == self.buffered.len() {
            let mut buffer = std::mem::take(&mut self.buffered);
            buffer.clear();
            let BufResult(result, buffer) = self.reader.read(buffer).await;
            self.buffered = buffer;
            self.offset = 0;
            if result? == 0 {
                return Ok(None);
            }
        }

        ::compio::time::timeout(COMMAND_TIMEOUT, self.complete_frame())
            .await
            .map_err(|_| FrameError::Timeout)?
            .map(Some)
    }

    pub async fn next_envelope(&mut self) -> Result<Option<Envelope>, FrameError> {
        as_envelope(self.next_frame().await?)
    }

    async fn complete_frame(&mut self) -> Result<Frame, FrameError> {
        let prefix = self.take_exact([0; 4]).await?;
        let length = frame_body_length(self.role, prefix)?;
        let [kind] = self.take_exact([0; 1]).await?;
        self.role.check(length, kind)?;
        let body_len = length - 1;
        if kind == STRUCTURED {
            return self
                .take_exact(vec![0; body_len])
                .await
                .map(Frame::Structured);
        }

        let data_len = body_len.checked_sub(4).ok_or(FrameError::RenderHeader)?;
        let header_length = self.take_exact([0; 4]).await?;
        let header_len = render_header_length(header_length, data_len)?;
        let header = self.take_exact(vec![0; header_len]).await?;
        let ansi_len = data_len - header_len;
        let header = decode_render_header(&header, ansi_len)?;
        let ansi = self.take_exact(vec![0; ansi_len]).await?;
        Ok(Frame::Render(RenderFrame { header, ansi }))
    }

    async fn take_exact<B: IoBufMut>(&mut self, bytes: B) -> Result<B, FrameError> {
        let mut buffered = &self.buffered[self.offset..];
        let BufResult(copied, mut bytes) = buffered.read(bytes).await;
        let copied = copied?;
        self.offset += copied;
        if copied == bytes.buf_capacity() {
            return Ok(bytes);
        }
        read_exact(&mut self.reader, bytes.slice(copied..))
            .await
            .map(IntoInner::into_inner)
    }
}

async fn read_exact<B: IoBufMut>(reader: &mut impl AsyncRead, bytes: B) -> Result<B, FrameError> {
    let BufResult(result, bytes) = reader.read_exact(bytes).await;
    match result {
        Ok(()) => Ok(bytes),
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => Err(FrameError::Truncated),
        Err(error) => Err(error.into()),
    }
}

/// A cancelled or failed frame write leaves the stream unusable.
pub struct CompioFrameWriter<W> {
    writer: W,
    role: FrameRole,
}

impl<W: AsyncWrite> CompioFrameWriter<W> {
    pub async fn start(mut writer: W, role: FrameRole) -> Result<Self, FrameError> {
        writer.write_all(FRAME_PREAMBLE).await.0?;
        writer.flush().await?;
        Ok(Self { writer, role })
    }

    pub async fn envelope(&mut self, envelope: &Envelope) -> Result<(), FrameError> {
        if envelope.encoded_len() >= self.role.structured_limit() {
            return Err(FrameError::TooLarge);
        }
        self.opaque_envelope(envelope.encode_to_vec()).await
    }

    pub async fn opaque_envelope(&mut self, payload: Vec<u8>) -> Result<(), FrameError> {
        let length = frame_length(payload.len(), 1)?;
        let prefix = frame_prefix(self.role, length, STRUCTURED)?;
        // Coalesce small control frames into a single submission. Preserve
        // ownership of larger payloads without allocating a second buffer.
        const INLINE_PAYLOAD: usize = 256;
        if payload.len() <= INLINE_PAYLOAD {
            let mut bytes = [0_u8; 5 + INLINE_PAYLOAD];
            let end = prefix.len() + payload.len();
            bytes[..prefix.len()].copy_from_slice(&prefix);
            bytes[prefix.len()..end].copy_from_slice(&payload);
            self.writer.write_all(bytes.slice(..end)).await.0?;
        } else {
            self.writer.write_all(prefix).await.0?;
            self.writer.write_all(payload).await.0?;
        }
        self.writer.flush().await?;
        Ok(())
    }

    pub async fn render(&mut self, frame: RenderFrame) -> Result<(), FrameError> {
        check_render_header(&frame.header, frame.ansi.len())?;
        let header_len = frame.header.encoded_len();
        if header_len > RENDER_HEADER_LIMIT {
            return Err(FrameError::RenderHeader);
        }

        let length = frame_length(frame.ansi.len(), 5 + header_len)?;
        let prefix = frame_prefix(self.role, length, RENDER)?;
        self.writer.write_all(prefix).await.0?;
        self.writer
            .write_all((header_len as u32).to_be_bytes())
            .await
            .0?;
        self.writer
            .write_all(frame.header.encode_to_vec())
            .await
            .0?;
        self.writer.write_all(frame.ansi).await.0?;
        self.writer.flush().await?;
        Ok(())
    }

    pub async fn shutdown(&mut self) -> Result<(), FrameError> {
        self.writer.shutdown().await?;
        Ok(())
    }
}
