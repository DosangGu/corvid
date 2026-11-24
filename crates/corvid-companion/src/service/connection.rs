use corvid_protocol::ProtocolFault;
use corvid_protocol::wire::CompanionConnection;
use uuid::Uuid;

use crate::remote::Remote;

pub(super) struct ReadySnapshot {
    pub(super) generation: Uuid,
    pub(super) remote: Remote,
}

pub(super) enum Connection {
    Connecting(Option<&'static str>),
    Syncing(Option<&'static str>),
    Ready(ReadySnapshot),
    Incompatible(&'static str),
    Offline(&'static str),
    Error(&'static str),
}

impl Connection {
    pub(super) fn ready(&self) -> Option<&ReadySnapshot> {
        match self {
            Self::Ready(snapshot) => Some(snapshot),
            _ => None,
        }
    }

    pub(super) fn status(&self) -> i32 {
        let status = match self {
            Self::Connecting(_) => CompanionConnection::Connecting,
            Self::Syncing(_) => CompanionConnection::Syncing,
            Self::Ready(_) => CompanionConnection::Ready,
            Self::Incompatible(_) => CompanionConnection::Incompatible,
            Self::Offline(_) => CompanionConnection::Offline,
            Self::Error(_) => CompanionConnection::Error,
        };
        status as i32
    }

    pub(super) fn issue(&self) -> Option<&'static str> {
        match *self {
            Self::Connecting(issue) | Self::Syncing(issue) => issue,
            Self::Incompatible(issue) | Self::Offline(issue) | Self::Error(issue) => Some(issue),
            Self::Ready(_) => None,
        }
    }

    pub(super) fn disconnected(error: &anyhow::Error) -> Self {
        match error
            .downcast_ref::<ProtocolFault>()
            .map(|fault| fault.code.as_str())
        {
            Some("HostMismatch") => Self::Incompatible("HostMismatch"),
            Some("ProtocolMismatch" | "UnsupportedCapability") => {
                Self::Incompatible("ProtocolMismatch")
            }
            Some("AuthenticationRequired") => Self::Error("AuthenticationRequired"),
            Some("HostKeyRequired") => Self::Error("HostKeyRequired"),
            Some("RelayUnavailable") => Self::Error("RelayUnavailable"),
            Some("InvalidMessage") => Self::Error("InvalidMessage"),
            Some("PersistenceFailed") => Self::Error("PersistenceFailed"),
            Some("LimitExceeded") => Self::Error("LimitExceeded"),
            Some("DaemonUnavailable") => Self::Offline("DaemonUnavailable"),
            Some("ResyncRequired") => Self::Syncing(Some("ResyncRequired")),
            _ => Self::Offline("ConnectionRequired"),
        }
    }
}
