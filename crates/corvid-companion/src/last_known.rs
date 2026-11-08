use corvid_protocol::wire::{
    AgentBadge, AgentCollection, AgentProvider, EvidenceConfidence, PresentedRun, RunAvailability,
};
use serde::{Deserialize, Serialize};

use crate::RunKey;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct LastKnown {
    badge: i32,
    provider: i32,
    confidence: i32,
    collection: i32,
    pending_requests: u32,
    unverified_requests: u32,
    unseen_attention: u32,
    last_response_unix_ms: Option<u64>,
    last_observed_unix_ms: Option<u64>,
}

impl LastKnown {
    pub(crate) fn from_display(run: &PresentedRun) -> Self {
        Self {
            badge: run.last_badge,
            provider: run.provider,
            confidence: run.confidence,
            collection: run.collection,
            pending_requests: run.pending_requests,
            unverified_requests: run.unverified_requests,
            unseen_attention: run.unseen_attention,
            last_response_unix_ms: run.last_response_unix_ms,
            last_observed_unix_ms: run.last_observed_unix_ms,
        }
    }

    pub(crate) fn display(self, key: RunKey) -> PresentedRun {
        PresentedRun {
            key: Some(key.into()),
            availability: RunAvailability::CheckNeeded as i32,
            last_badge: self.badge,
            provider: self.provider,
            confidence: self.confidence,
            collection: self.collection,
            pending_requests: self.pending_requests,
            unverified_requests: self.unverified_requests,
            unseen_attention: self.unseen_attention,
            latest_unseen_attention_id: None,
            last_response_unix_ms: self.last_response_unix_ms,
            last_observed_unix_ms: self.last_observed_unix_ms,
        }
    }

    pub(crate) fn is_valid(self) -> bool {
        self.badge != 0
            && AgentBadge::try_from(self.badge).is_ok()
            && self.provider != 0
            && AgentProvider::try_from(self.provider).is_ok()
            && EvidenceConfidence::try_from(self.confidence).is_ok()
            && self.collection != 0
            && AgentCollection::try_from(self.collection).is_ok()
    }
}
