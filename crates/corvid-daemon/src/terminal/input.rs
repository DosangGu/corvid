use std::collections::VecDeque;
use std::io::{ErrorKind, Write};
use std::time::{Duration, Instant};

use anyhow::{Result, bail, ensure};

const USER_LIMIT: usize = 128 * 1024;
const REPLY_LIMIT: usize = 64 * 1024;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(super) struct InputQueue {
    user: VecDeque<Vec<u8>>,
    replies: VecDeque<Vec<u8>>,
    user_bytes: usize,
    reply_bytes: usize,
    active: Option<Batch>,
}

struct Batch {
    bytes: Vec<u8>,
    sent: usize,
    user: bool,
    progress: Instant,
}

impl InputQueue {
    pub(super) fn user(&mut self, bytes: Vec<u8>) -> bool {
        if self.user_bytes + bytes.len() > USER_LIMIT {
            return false;
        }
        self.user_bytes += bytes.len();
        self.user.push_back(bytes);
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
        if self.active.as_ref().is_some_and(|batch| batch.user) {
            self.active = None;
        }
    }

    pub(super) fn flush(&mut self, writer: &mut impl Write) -> Result<()> {
        for _ in 0..16 {
            if self.active.is_none() {
                self.active = self.next_batch();
            }
            let Some(batch) = &mut self.active else {
                return Ok(());
            };
            match writer.write(&batch.bytes[batch.sent..]) {
                Ok(0) => bail!("PTY write made no progress"),
                Ok(written) => {
                    batch.sent += written;
                    batch.progress = Instant::now();
                    if batch.sent == batch.bytes.len() {
                        if batch.user {
                            self.user_bytes -= batch.bytes.len();
                        } else {
                            self.reply_bytes -= batch.bytes.len();
                        }
                        self.active = None;
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    ensure!(
                        batch.progress.elapsed() < WRITE_TIMEOUT,
                        "PTY input stalled"
                    );
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn next_batch(&mut self) -> Option<Batch> {
        let (bytes, user) = if let Some(reply) = self.replies.pop_front() {
            (reply, false)
        } else {
            (self.user.pop_front()?, true)
        };
        Some(Batch {
            bytes,
            sent: 0,
            user,
            progress: Instant::now(),
        })
    }
}
