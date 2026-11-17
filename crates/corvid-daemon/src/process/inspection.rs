//! OS observations, not liveness or provider policy.
//!
//! Handles must never silently follow a reused PID. They are not snapshots:
//! exec, foreground groups, and directories can change between calls. Callers
//! must compare the birth token and executable when establishing ownership.

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{Executable, Stat};

pub(super) const ARGUMENT_LIMIT: usize = 4096;

/// Equality evidence within a boot, not a portable timestamp or duration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) struct BirthIdentity(u64);

impl BirthIdentity {
    pub(crate) fn from_raw(value: u64) -> Self {
        Self(value)
    }

    pub(super) fn verify(self, expected: Self) -> Observation<()> {
        (self == expected)
            .then_some(())
            .ok_or(ObservationError::Changed)
    }

    /// Preserve the existing wire field without assigning it a time unit.
    pub(crate) fn wire_value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ObservationError {
    /// The queried process/entry is absent. Only an absent process at open is
    /// sufficient for the existing shell-exit policy; a missing exe is not.
    Gone,
    /// The PID now names a different birth identity.
    Changed,
    /// Access denied, interrupted exec, or another inconclusive query failure.
    Unavailable,
    /// Malformed data or a path whose filesystem identity cannot be confirmed.
    Invalid,
    TooLarge,
}

impl From<io::Error> for ObservationError {
    fn from(error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::NotFound {
            Self::Gone
        } else {
            Self::Unavailable
        }
    }
}

impl From<nix::errno::Errno> for ObservationError {
    fn from(error: nix::errno::Errno) -> Self {
        io::Error::from(error).into()
    }
}

pub(super) type Observation<T> = Result<T, ObservationError>;

pub(super) trait ProcessInspector {
    type Handle: ProcessObservation;

    /// Bind a handle to the process currently named by pid. A successful open
    /// alone does not establish that it is the process the caller expects.
    fn open(&self, pid: u32) -> Observation<Self::Handle>;

    fn helper_image(&self) -> Option<Executable> {
        Executable::read(&std::env::current_exe().ok()?)
    }
}

pub(super) trait ProcessObservation {
    fn stat(&self) -> Observation<Stat>;
    fn executable(&self) -> Observation<Executable>;
    fn path(&self) -> Observation<PathBuf>;

    /// NUL-separated argv, at most ARGUMENT_LIMIT bytes; never a truncated
    /// prefix accepted as a complete command. No environment is returned.
    fn arguments(&self) -> Observation<Vec<u8>>;

    /// Return a directory only after its named device/inode matches the
    /// observed directory. Deleted or replaced paths must not be published.
    fn cwd(&self) -> Observation<PathBuf>;
}

pub(super) trait Clock {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
}

pub(super) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}
