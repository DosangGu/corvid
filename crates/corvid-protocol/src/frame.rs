use std::io::{self, ErrorKind};

use prost::Message;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::preflight;
use crate::session::valid_id;
use crate::wire::{Envelope, RenderHeader};

pub const FRAME_PREAMBLE: [u8; 4] = *b"CRV1";
pub const CONTROL_LIMIT: usize = 256 * 1024;
const COLLECTOR_LIMIT: usize = 8 * 1024;
pub const RENDER_LIMIT: usize = 4 * 1024 * 1024;

const STRUCTURED: u8 = 1;
const RENDER: u8 = 2;
const RENDER_HEADER_LIMIT: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameRole {
    StructuredOnly,
    Collector,
    TuiOutput,
}

impl FrameRole {
    fn limit(self) -> usize {
        match self {
            Self::StructuredOnly => CONTROL_LIMIT,
            Self::Collector => COLLECTOR_LIMIT,
            Self::TuiOutput => RENDER_LIMIT,
        }
    }

    fn check(self, length: usize, kind: u8) -> Result<(), FrameError> {
        match kind {
            STRUCTURED if length > self.structured_limit() => Err(FrameError::TooLarge),
            STRUCTURED => Ok(()),
            RENDER if self != Self::TuiOutput => Err(FrameError::WrongRole),
            RENDER if length > RENDER_LIMIT => Err(FrameError::TooLarge),
            RENDER => Ok(()),
            _ => Err(FrameError::UnknownKind(kind)),
        }
    }

    fn structured_limit(self) -> usize {
        match self {
            Self::Collector => COLLECTOR_LIMIT,
            _ => CONTROL_LIMIT,
        }
    }
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("unsupported framing preamble")]
    ProtocolMismatch,
    #[error("zero frame length")]
    EmptyFrame,
    #[error("frame exceeds the size limit")]
    TooLarge,
    #[error("frame ended before its declared length")]
    Truncated,
    #[error("unknown frame kind {0}")]
    UnknownKind(u8),
    #[error("frame kind forbidden for this stream")]
    WrongRole,
    #[error("invalid render header")]
    RenderHeader,
    #[error("malformed Protobuf field")]
    MalformedField,
    #[error("Protobuf nesting limit exceeded")]
    NestingLimit,
    #[error("Protobuf collection limit exceeded")]
    CollectionLimit,
    #[error("malformed Protobuf")]
    Protobuf(#[from] prost::DecodeError),
    #[error("stream I/O error")]
    Io(#[from] io::Error),
}

#[derive(Debug, PartialEq)]
pub enum Frame {
    /// Opaque bytes so a relay can retain fields from newer compatible peers.
    Structured(Vec<u8>),
    Render(RenderFrame),
}

#[derive(Debug, PartialEq)]
pub struct RenderFrame {
    pub header: RenderHeader,
    pub ansi: Vec<u8>,
}

pub fn decode_envelope(payload: &[u8]) -> Result<Envelope, FrameError> {
    if payload.len() >= CONTROL_LIMIT {
        return Err(FrameError::TooLarge);
    }

    preflight::envelope(payload)?;
    Ok(Envelope::decode(payload)?)
}

/// A cancelled or failed frame read leaves the stream unusable.
pub struct FrameReader<R> {
    reader: R,
    role: FrameRole,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub async fn start(mut reader: R, role: FrameRole) -> Result<Self, FrameError> {
        let mut preamble = [0; 4];
        read_exact(&mut reader, &mut preamble).await?;
        if preamble != FRAME_PREAMBLE {
            return Err(FrameError::ProtocolMismatch);
        }

        Ok(Self { reader, role })
    }

    pub async fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        let mut length = [0; 4];
        if self.reader.read(&mut length[..1]).await? == 0 {
            return Ok(None);
        }

        read_exact(&mut self.reader, &mut length[1..]).await?;
        let length = u32::from_be_bytes(length) as usize;
        if length == 0 {
            return Err(FrameError::EmptyFrame);
        }
        if length > self.role.limit() {
            return Err(FrameError::TooLarge);
        }

        let mut kind = [0];
        read_exact(&mut self.reader, &mut kind).await?;
        self.role.check(length, kind[0])?;

        let body_len = length - 1;
        let frame = if kind[0] == STRUCTURED {
            let mut payload = vec![0; body_len];
            read_exact(&mut self.reader, &mut payload).await?;
            Frame::Structured(payload)
        } else {
            Frame::Render(read_render(&mut self.reader, body_len).await?)
        };
        Ok(Some(frame))
    }

    pub async fn next_envelope(&mut self) -> Result<Option<Envelope>, FrameError> {
        match self.next_frame().await? {
            Some(Frame::Structured(payload)) => decode_envelope(&payload).map(Some),
            Some(Frame::Render(_)) => Err(FrameError::WrongRole),
            None => Ok(None),
        }
    }
}

async fn read_render(
    reader: &mut (impl AsyncRead + Unpin),
    body_len: usize,
) -> Result<RenderFrame, FrameError> {
    let data_len = body_len.checked_sub(4).ok_or(FrameError::RenderHeader)?;
    let mut header_len = [0; 4];
    read_exact(reader, &mut header_len).await?;
    let header_len = u32::from_be_bytes(header_len) as usize;
    if header_len == 0 || header_len > RENDER_HEADER_LIMIT || header_len > data_len {
        return Err(FrameError::RenderHeader);
    }

    let mut bytes = vec![0; header_len];
    read_exact(reader, &mut bytes).await?;
    preflight::render_header(&bytes)?;
    let header = RenderHeader::decode(bytes.as_slice())?;

    let ansi_len = data_len - header_len;
    check_render_header(&header, ansi_len)?;
    let mut ansi = vec![0; ansi_len];
    read_exact(reader, &mut ansi).await?;
    Ok(RenderFrame { header, ansi })
}

async fn read_exact(
    reader: &mut (impl AsyncRead + Unpin),
    bytes: &mut [u8],
) -> Result<(), FrameError> {
    match reader.read_exact(bytes).await {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => Err(FrameError::Truncated),
        Err(error) => Err(error.into()),
    }
}

fn check_render_header(header: &RenderHeader, ansi_len: usize) -> Result<(), FrameError> {
    if header.ansi_bytes as usize != ansi_len
        || !valid_id(&header.host_id)
        || !valid_id(&header.server_epoch)
        || !valid_id(&header.client_id)
        || header.render_generation == 0
        || header.frame_id == 0
        || header
            .base_frame_id
            .is_some_and(|base| base == 0 || base >= header.frame_id)
    {
        return Err(FrameError::RenderHeader);
    }
    Ok(())
}

/// A cancelled or failed frame write leaves the stream unusable.
pub struct FrameWriter<W> {
    writer: W,
    role: FrameRole,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    pub async fn start(mut writer: W, role: FrameRole) -> Result<Self, FrameError> {
        writer.write_all(&FRAME_PREAMBLE).await?;
        writer.flush().await?;
        Ok(Self { writer, role })
    }

    pub async fn envelope(&mut self, envelope: &Envelope) -> Result<(), FrameError> {
        if envelope.encoded_len() >= self.role.structured_limit() {
            return Err(FrameError::TooLarge);
        }

        self.opaque_envelope(&envelope.encode_to_vec()).await
    }

    pub async fn opaque_envelope(&mut self, payload: &[u8]) -> Result<(), FrameError> {
        let length = frame_length(payload.len(), 1)?;
        self.prefix(length, STRUCTURED).await?;
        self.writer.write_all(payload).await?;
        self.writer.flush().await?;
        Ok(())
    }

    pub async fn render(&mut self, frame: &RenderFrame) -> Result<(), FrameError> {
        check_render_header(&frame.header, frame.ansi.len())?;
        let header_len = frame.header.encoded_len();
        if header_len > RENDER_HEADER_LIMIT {
            return Err(FrameError::RenderHeader);
        }

        let length = frame_length(frame.ansi.len(), 5 + header_len)?;
        self.prefix(length, RENDER).await?;
        self.writer.write_u32(header_len as u32).await?;
        self.writer.write_all(&frame.header.encode_to_vec()).await?;
        self.writer.write_all(&frame.ansi).await?;
        self.writer.flush().await?;
        Ok(())
    }

    pub async fn shutdown(&mut self) -> Result<(), FrameError> {
        self.writer.shutdown().await?;
        Ok(())
    }

    async fn prefix(&mut self, length: usize, kind: u8) -> Result<(), FrameError> {
        self.role.check(length, kind)?;
        let length = u32::try_from(length).map_err(|_| FrameError::TooLarge)?;
        let mut prefix = [kind; 5];
        prefix[..4].copy_from_slice(&length.to_be_bytes());
        self.writer.write_all(&prefix).await?;
        Ok(())
    }
}

fn frame_length(payload: usize, overhead: usize) -> Result<usize, FrameError> {
    payload.checked_add(overhead).ok_or(FrameError::TooLarge)
}
