use std::collections::VecDeque;

use base64::{Engine, engine::general_purpose::STANDARD};
use corvid_protocol::{CLIPBOARD_WRITE_MAX_BYTES, valid_clipboard_text};

const ENCODED_MAX: usize = CLIPBOARD_WRITE_MAX_BYTES.div_ceil(3) * 4;
const SEQUENCE_LIMIT: usize = ENCODED_MAX + b"\x1b]52;c;\x07".len();

#[derive(Default)]
pub(super) struct Clipboard {
    tail: VecDeque<u8>,
    awaiting_st: Option<String>,
    latest: Option<String>,
}

impl Clipboard {
    pub(super) fn advance(&mut self, bytes: &[u8], dispatched: bool) {
        if let Some(text) = self.awaiting_st.take()
            && bytes.first() == Some(&b'\\')
        {
            self.latest = Some(text);
        }

        // VTE identifies the OSC boundary. Use the original bytes here because
        // its fixed buffer can truncate the Base64 and omit control bytes.
        let bytes = &bytes[bytes.len().saturating_sub(SEQUENCE_LIMIT)..];
        let excess = (self.tail.len() + bytes.len()).saturating_sub(SEQUENCE_LIMIT);
        self.tail.drain(..excess);
        self.tail.extend(bytes);
        if dispatched {
            match self.tail.back() {
                Some(b'\x07') => {
                    if let Some(text) = self.decode() {
                        self.latest = Some(text);
                    }
                }
                Some(b'\x1b') => self.awaiting_st = self.decode(),
                _ => {}
            }
        }
    }

    fn decode(&mut self) -> Option<String> {
        let bytes = self.tail.make_contiguous();
        let body = bytes.get(..bytes.len().checked_sub(1)?)?;
        let start = body.iter().rposition(|byte| *byte == b'\x1b')?;
        let payload = body[start..].strip_prefix(b"\x1b]52;")?;
        let encoded = payload
            .strip_prefix(b"c;")
            .or_else(|| payload.strip_prefix(b";"))?;
        if encoded.is_empty() || encoded.len() > ENCODED_MAX {
            return None;
        }
        let text = String::from_utf8(STANDARD.decode(encoded).ok()?).ok()?;
        valid_clipboard_text(&text).then_some(text)
    }

    pub(super) fn take(&mut self) -> Option<String> {
        self.latest.take()
    }
}
