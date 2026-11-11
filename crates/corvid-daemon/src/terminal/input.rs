use std::collections::VecDeque;
use std::io::{ErrorKind, Write};
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use corvid_core::{MAX_PASTE_BYTES, PASTE_END, PASTE_START};

const USER_LIMIT: usize = 128 * 1024;
const REPLY_LIMIT: usize = 64 * 1024;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(super) struct InputQueue {
    user: VecDeque<(Vec<u8>, Kind)>,
    replies: VecDeque<Vec<u8>>,
    user_bytes: usize,
    reply_bytes: usize,
    active: Option<Batch>,
    paste_pending: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Keys,
    Paste,
    Reply,
}

#[derive(Clone, Copy)]
pub(super) struct WriteFailure {
    pub(super) kind: Kind,
    pub(super) partial: bool,
}

struct Batch {
    bytes: Vec<u8>,
    sent: usize,
    kind: Kind,
    progress: Instant,
}

impl InputQueue {
    pub(super) fn user(&mut self, bytes: Vec<u8>) -> bool {
        if self.user_bytes + bytes.len() > USER_LIMIT {
            return false;
        }
        self.user_bytes += bytes.len();
        self.user.push_back((bytes, Kind::Keys));
        true
    }

    pub(super) fn paste_pending(&self) -> bool {
        self.paste_pending
    }

    pub(super) fn interrupted_paste(&self) -> Option<WriteFailure> {
        self.paste_pending.then(|| WriteFailure {
            kind: Kind::Paste,
            partial: self
                .active
                .as_ref()
                .is_some_and(|batch| batch.kind == Kind::Paste && batch.sent > 0),
        })
    }

    pub(super) fn paste(&mut self, bytes: Vec<u8>, bracketed: bool) -> bool {
        if self.paste_pending || bytes.len() > MAX_PASTE_BYTES {
            return false;
        }
        let bytes = if bracketed {
            [
                PASTE_START.as_slice(),
                bytes.as_slice(),
                PASTE_END.as_slice(),
            ]
            .concat()
        } else {
            bytes
        };
        if !bytes.is_empty() {
            self.user.push_back((bytes, Kind::Paste));
            self.paste_pending = true;
        }
        true
    }

    pub(super) fn reply(&mut self, bytes: Vec<u8>) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        ensure!(
            self.reply_bytes + bytes.len() <= REPLY_LIMIT,
            "terminal reply queue exhausted"
        );
        self.reply_bytes += bytes.len();
        self.replies.push_back(bytes);
        Ok(())
    }

    pub(super) fn fence(&mut self) {
        self.user.clear();
        self.user_bytes = 0;
        self.paste_pending = false;
        if self
            .active
            .as_ref()
            .is_some_and(|batch| batch.kind != Kind::Reply)
        {
            self.active = None;
        }
    }

    pub(super) fn flush(&mut self, writer: &mut impl Write) -> Result<(), WriteFailure> {
        for _ in 0..16 {
            if self.active.is_none() {
                self.active = self.next_batch();
            }
            let Some(batch) = &mut self.active else {
                return Ok(());
            };
            match writer.write(&batch.bytes[batch.sent..]) {
                Ok(0) => return Err(batch.failure()),
                Ok(written) => {
                    self.record_written(written);
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error)
                    if error.kind() == ErrorKind::WouldBlock
                        && batch.progress.elapsed() < WRITE_TIMEOUT =>
                {
                    return Ok(());
                }
                Err(_) => return Err(batch.failure()),
            }
        }
        Ok(())
    }

    pub(super) fn record_written(&mut self, written: usize) {
        let batch = self.active.as_mut().expect("PTY write has an active batch");
        batch.sent += written;
        batch.progress = Instant::now();
        if batch.sent == batch.bytes.len() {
            match batch.kind {
                Kind::Keys => self.user_bytes -= batch.bytes.len(),
                Kind::Reply => self.reply_bytes -= batch.bytes.len(),
                Kind::Paste => self.paste_pending = false,
            }
            self.active = None;
        }
    }

    fn next_batch(&mut self) -> Option<Batch> {
        let (bytes, kind) = if let Some(reply) = self.replies.pop_front() {
            (reply, Kind::Reply)
        } else {
            self.user.pop_front()?
        };
        Some(Batch {
            bytes,
            sent: 0,
            kind,
            progress: Instant::now(),
        })
    }
}

impl Batch {
    fn failure(&self) -> WriteFailure {
        WriteFailure {
            kind: self.kind,
            partial: self.sent > 0,
        }
    }
}
