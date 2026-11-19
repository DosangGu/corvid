use std::io;
use std::time::Duration;

use corvid_protocol::{FrameError, ProtocolFault};
use tokio::time::Instant;
use uuid::Uuid;

/// The deadline covers backoff and connection setup for one SSH outage.
pub struct SshRetry {
    deadline: Instant,
    delay: Duration,
}

impl Default for SshRetry {
    fn default() -> Self {
        Self {
            deadline: Instant::now() + Duration::from_secs(60),
            delay: Duration::from_millis(250),
        }
    }
}

impl SshRetry {
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn next_attempt(&mut self, error: &anyhow::Error) -> Option<Instant> {
        self.next_attempt_at(error, Instant::now())
    }

    fn next_attempt_at(&mut self, error: &anyhow::Error, now: Instant) -> Option<Instant> {
        if now >= self.deadline || !Self::is_transient(error) {
            return None;
        }
        let jitter = Uuid::new_v4().as_u128() as u64 % (self.delay.as_millis() as u64 / 2);
        let next = now + self.delay + Duration::from_millis(jitter);
        self.delay = (self.delay * 2).min(Duration::from_secs(20));
        Some(next.min(self.deadline))
    }

    pub fn is_transient(error: &anyhow::Error) -> bool {
        if let Some(fault) = error.downcast_ref::<ProtocolFault>() {
            return matches!(
                fault.code.as_str(),
                "Offline" | "DaemonUnavailable" | "ResyncRequired" | "ConnectionRequired"
            );
        }
        error.is::<tokio::time::error::Elapsed>()
            || matches!(
                error.downcast_ref::<FrameError>(),
                Some(FrameError::Timeout | FrameError::Truncated)
            )
            || error.chain().any(|cause| {
                cause.downcast_ref::<io::Error>().is_some_and(|error| {
                    matches!(
                        error.kind(),
                        io::ErrorKind::BrokenPipe
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::ConnectionRefused
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::NotConnected
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::UnexpectedEof
                    )
                })
            })
    }

    pub fn exhausted() -> ProtocolFault {
        ProtocolFault::new("Offline", "SSH is offline. Automatic reconnect stopped.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_transport_failures_only_until_the_fixed_deadline() {
        let mut retry = SshRetry::default();
        let offline = ProtocolFault::new("Offline", "disconnected").into();
        let reset = FrameError::Io(io::ErrorKind::ConnectionReset.into()).into();
        assert!(retry.next_attempt(&offline).unwrap() < retry.deadline());
        assert!(retry.next_attempt(&reset).unwrap() < retry.deadline());
        for code in [
            "AuthenticationRequired",
            "HostKeyRequired",
            "HostMismatch",
            "EpochMismatch",
            "ProtocolMismatch",
        ] {
            assert!(
                retry
                    .next_attempt(&ProtocolFault::new(code, "failed").into())
                    .is_none()
            );
        }
        let deadline = retry.deadline();
        let near_deadline = deadline - Duration::from_millis(1);
        assert_eq!(
            retry.next_attempt_at(&offline, near_deadline),
            Some(deadline)
        );
        assert!(retry.next_attempt_at(&offline, deadline).is_none());
    }
}
