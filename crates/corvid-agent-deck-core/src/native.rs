use std::fmt;
use std::future::Future;

use crate::notifications::Notice;

/// Availability of one service in this instance. Ready means requests can be
/// attempted; OS policy can still reject an individual request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Availability {
    #[default]
    Unsupported,
    Starting,
    Ready,
    Unavailable,
}

impl Availability {
    pub fn ready(self) -> bool {
        self == Self::Ready
    }

    pub fn check(self) -> Result<(), NativeError> {
        match self {
            Self::Unsupported => Err(NativeError::Unsupported),
            Self::Starting | Self::Unavailable => Err(NativeError::Unavailable),
            Self::Ready => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub status_icon: Availability,
    pub notifications: Availability,
    pub activation: Availability,
    pub sound: Availability,
    pub login_startup: Availability,
}

impl Capabilities {
    pub fn stopped(self) -> Self {
        self.map(|state| match state {
            Availability::Unsupported => state,
            _ => Availability::Unavailable,
        })
    }

    pub fn startup_timed_out(self) -> Self {
        self.map(|state| match state {
            Availability::Starting => Availability::Unavailable,
            _ => state,
        })
    }

    fn map(self, map: impl Fn(Availability) -> Availability) -> Self {
        Self {
            status_icon: map(self.status_icon),
            notifications: map(self.notifications),
            activation: map(self.activation),
            sound: map(self.sound),
            login_startup: map(self.login_startup),
        }
    }

    pub fn can_start_hidden(self) -> bool {
        matches!(
            self.status_icon,
            Availability::Starting | Availability::Ready
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeError {
    Unsupported,
    Unavailable,
    QueueFull,
    Disconnected,
    TimedOut,
    Rejected(String),
}

impl fmt::Display for NativeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str("Native service is not supported"),
            Self::Unavailable => f.write_str("Native service is unavailable"),
            Self::QueueFull => f.write_str("Native service queue is full"),
            Self::Disconnected => f.write_str("Native service disconnected"),
            Self::TimedOut => f.write_str("Native service result timed out"),
            Self::Rejected(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for NativeError {}

#[derive(Debug)]
pub enum Event {
    OpenNotifications,
    Activate(Notice),
    TogglePause,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub text: String,
    pub attention: bool,
}

/// Local to the monitor worker. Adapters may dispatch to their native owner.
/// Dropping an adapter closes its callbacks and releases its native resources.
pub trait NativeServices {
    /// Accept a status update for the icon. Pending status can be replaced.
    fn update_status(&mut self, status: Status) -> Result<(), NativeError>;

    /// Complete after the OS accepts or rejects the request. Acceptance doesn't
    /// imply a banner was shown. Activation is a separate event.
    fn notify(
        &mut self,
        notice: Notice,
        sound: bool,
    ) -> impl Future<Output = Result<(), NativeError>>;

    /// Complete after the registration operation. A timeout leaves its outcome
    /// unknown. Unsupported requests must return an error.
    fn set_login_startup(&mut self, enabled: bool)
    -> impl Future<Output = Result<(), NativeError>>;
}
