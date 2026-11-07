use std::time::Duration;

use anyhow::{Result, bail, ensure};
use corvid_core::{KEY_LEADER, MAX_PASTE_BYTES, PASTE_CAPTURE_TIMEOUT, PASTE_END, PASTE_START};
use tokio::time::Instant;

const ESC: u8 = 0x1b;
const KEY_LIMIT: usize = 64;

pub(crate) enum Input {
    Keys(Vec<u8>),
    Paste(Vec<u8>),
}

#[cfg(test)]
impl Input {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Keys(bytes) | Self::Paste(bytes) => bytes,
        }
    }
}

pub(crate) struct Decoder {
    escape_delay: Duration,
    key: Option<Key>,
    paste: Option<PasteCapture>,
    isolate_next: bool,
}

impl Decoder {
    pub(crate) fn new(escape_delay: Duration) -> Self {
        Self {
            escape_delay,
            key: None,
            paste: None,
            isolate_next: false,
        }
    }

    pub(crate) fn capturing_paste(&self) -> bool {
        self.paste.is_some()
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.key
            .as_ref()
            .map(|key| key.deadline)
            .or_else(|| self.paste.as_ref().map(|paste| paste.deadline))
    }

    pub(crate) fn retire(&mut self) {
        self.isolate_next = false;
        if let Some(key) = &mut self.key {
            key.retire();
        }
        if let Some(paste) = &mut self.paste {
            paste.discard = true;
            paste.data.clear();
        }
    }

    pub(crate) fn expire(&mut self) -> Result<Vec<Input>> {
        let mut output = Vec::new();
        if self
            .deadline()
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            if self.paste.is_some() {
                bail!("paste did not finish within ten seconds");
            }
            if let Some(key) = self.key.take() {
                ensure!(!key.discard, "retired key sequence did not finish");
                self.emit_key(key.bytes, &mut output);
            }
        }
        Ok(output)
    }

    pub(crate) fn feed(&mut self, bytes: &[u8], accepting: bool) -> Result<Vec<Input>> {
        let mut output = self.expire()?;
        let mut plain = Vec::new();
        for &byte in bytes {
            if let Some(paste) = &mut self.paste {
                if paste.push(byte)? {
                    let paste = self.paste.take().expect("finished capture is present");
                    if !paste.discard {
                        output.push(Input::Paste(paste.data));
                    }
                }
                continue;
            }
            if let Some(mut key) = self.key.take() {
                match key.push(byte) {
                    Step::More => {
                        self.key = Some(key);
                        continue;
                    }
                    Step::Complete => {
                        self.finish_key(key, &mut output);
                        continue;
                    }
                    Step::Interrupted => self.finish_key(key, &mut output),
                }
            }
            if let Some(kind) = KeyKind::start(byte) {
                flush_plain(&mut plain, &mut output);
                let mut key = Key {
                    kind,
                    bytes: vec![byte],
                    deadline: Instant::now() + self.escape_delay,
                    discard: false,
                };
                if !accepting {
                    key.retire();
                }
                self.key = Some(key);
            } else if accepting {
                if byte.is_ascii_control() || self.isolate_next {
                    flush_plain(&mut plain, &mut output);
                    self.emit_key(vec![byte], &mut output);
                } else {
                    plain.push(byte);
                }
            }
        }
        flush_plain(&mut plain, &mut output);
        Ok(output)
    }

    fn finish_key(&mut self, key: Key, output: &mut Vec<Input>) {
        if key.bytes == PASTE_START.as_slice() {
            self.isolate_next = false;
            self.paste = Some(PasteCapture {
                data: Vec::new(),
                matched: 0,
                deadline: Instant::now() + PASTE_CAPTURE_TIMEOUT,
                discard: key.discard,
            });
        } else if !key.discard {
            self.emit_key(key.bytes, output);
        }
    }

    fn emit_key(&mut self, bytes: Vec<u8>, output: &mut Vec<Input>) {
        self.isolate_next = bytes == [KEY_LEADER] && !self.isolate_next;
        output.push(Input::Keys(bytes));
    }
}

fn flush_plain(plain: &mut Vec<u8>, output: &mut Vec<Input>) {
    if !plain.is_empty() {
        output.push(Input::Keys(std::mem::take(plain)));
    }
}

enum KeyKind {
    Escape,
    Legacy,
    Utf8(u8),
}

impl KeyKind {
    fn start(byte: u8) -> Option<Self> {
        match byte {
            ESC => Some(Self::Escape),
            0xc2..=0xdf => Some(Self::Utf8(1)),
            0xe0..=0xef => Some(Self::Utf8(2)),
            0xf0..=0xf4 => Some(Self::Utf8(3)),
            _ => None,
        }
    }
}

struct Key {
    kind: KeyKind,
    bytes: Vec<u8>,
    deadline: Instant,
    discard: bool,
}

enum Step {
    More,
    Complete,
    Interrupted,
}

impl Key {
    fn retire(&mut self) {
        if !self.discard {
            self.discard = true;
            self.deadline = Instant::now() + PASTE_CAPTURE_TIMEOUT;
        }
    }

    fn push(&mut self, byte: u8) -> Step {
        if byte == ESC {
            return Step::Interrupted;
        }
        let step = match self.kind {
            KeyKind::Escape if matches!(byte, b'[' | b'O') => {
                self.kind = KeyKind::Legacy;
                Step::More
            }
            KeyKind::Escape => match KeyKind::start(byte) {
                Some(kind) => {
                    self.kind = kind;
                    Step::More
                }
                None => Step::Complete,
            },
            KeyKind::Legacy if (0x20..=0x3f).contains(&byte) => Step::More,
            KeyKind::Legacy => Step::Complete,
            KeyKind::Utf8(_) if !(0x80..=0xbf).contains(&byte) => return Step::Interrupted,
            KeyKind::Utf8(1) => Step::Complete,
            KeyKind::Utf8(remaining) => {
                self.kind = KeyKind::Utf8(remaining - 1);
                Step::More
            }
        };
        self.bytes.push(byte);
        if self.bytes.len() >= KEY_LIMIT {
            return Step::Complete;
        }
        step
    }
}

struct PasteCapture {
    data: Vec<u8>,
    matched: usize,
    deadline: Instant,
    discard: bool,
}

impl PasteCapture {
    fn push(&mut self, byte: u8) -> Result<bool> {
        if byte == PASTE_END[self.matched] {
            self.matched += 1;
            return Ok(self.matched == PASTE_END.len());
        }
        if !self.discard {
            self.data.extend_from_slice(&PASTE_END[..self.matched]);
        }
        self.matched = usize::from(byte == ESC);
        if !self.discard && self.matched == 0 {
            self.data.push(byte);
        }
        ensure!(
            self.data.len() <= MAX_PASTE_BYTES,
            "paste exceeds the 1 MiB limit"
        );
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoder() -> Decoder {
        Decoder::new(Duration::from_millis(100))
    }

    #[test]
    fn preserves_escape_utf8_and_leader_boundaries_across_reads() {
        let mut decoder = decoder();
        assert!(decoder.feed(b"\x1b", true).unwrap().is_empty());
        assert_eq!(decoder.feed(b"x", true).unwrap()[0].bytes(), b"\x1bx");
        assert!(decoder.feed(b"\x1b[1;", true).unwrap().is_empty());
        assert_eq!(decoder.feed(b"5A", true).unwrap()[0].bytes(), b"\x1b[1;5A");

        let wide = "한".as_bytes();
        assert!(decoder.feed(&wide[..2], true).unwrap().is_empty());
        assert_eq!(decoder.feed(&wide[2..], true).unwrap()[0].bytes(), wide);

        let command = decoder.feed(b"\x1ddtail", true).unwrap();
        assert_eq!(
            command.iter().map(Input::bytes).collect::<Vec<_>>(),
            [b"\x1d".as_slice(), b"d", b"tail"]
        );

        assert!(decoder.feed(b"\x1b", true).unwrap().is_empty());
        decoder.key.as_mut().unwrap().deadline = Instant::now();
        assert_eq!(decoder.expire().unwrap()[0].bytes(), b"\x1b");
    }

    #[test]
    fn keeps_paste_literal_and_retires_a_fragmented_old_route_marker() {
        let mut decoder = decoder();
        assert!(decoder.feed(b"\x1b[20", true).unwrap().is_empty());
        decoder.retire();
        assert!(decoder.feed(b"0~ab\x1dd\x1b[201", true).unwrap().is_empty());
        let output = decoder.feed(b"~fresh", true).unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].bytes(), b"fresh");

        assert!(
            decoder
                .feed(b"\x1b[200~paste\x1dd\x1b[20X\x1b", true)
                .unwrap()
                .is_empty()
        );
        let mut output = decoder.feed(b"[201~", true).unwrap();
        let Some(Input::Paste(bytes)) = output.pop() else {
            panic!("expected paste transaction")
        };
        assert_eq!(bytes, b"paste\x1dd\x1b[20X");
    }
}
