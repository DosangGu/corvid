use std::time::Instant;

use corvid_core::{InputRoute, MAX_PASTE_BYTES, PASTE_CAPTURE_TIMEOUT};
use corvid_protocol::ProtocolFault;
use uuid::Uuid;

pub(super) struct Paste {
    id: Uuid,
    route: InputRoute,
    started: Instant,
    bytes: Vec<u8>,
    pub(super) request_id: Option<String>,
}

impl Paste {
    pub(super) fn new(id: Uuid, route: InputRoute, request_id: Option<String>) -> Self {
        Self {
            id,
            route,
            started: Instant::now(),
            bytes: Vec::new(),
            request_id,
        }
    }

    pub(super) fn expired(&self) -> bool {
        self.started.elapsed() >= PASTE_CAPTURE_TIMEOUT
    }

    pub(super) fn check(&self, id: Uuid, route: InputRoute) -> Result<(), ProtocolFault> {
        if self.expired() {
            return Err(ProtocolFault::new(
                "Timeout",
                "paste expired before completion",
            ));
        }
        if self.id != id || self.route != route {
            return Err(ProtocolFault::new(
                "StaleRoute",
                "paste belongs to a different input route",
            ));
        }
        Ok(())
    }

    pub(super) fn append(&mut self, bytes: &[u8]) -> Result<(), ProtocolFault> {
        if self.bytes.len() + bytes.len() > MAX_PASTE_BYTES {
            return Err(ProtocolFault::new(
                "LimitExceeded",
                "paste exceeds the 1 MiB limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    pub(super) fn finish(self) -> Vec<u8> {
        self.bytes
    }
}
