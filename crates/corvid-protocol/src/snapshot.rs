mod cache;
mod validate;

use crate::wire::snapshot_entity::Body;
use crate::wire::{EntityKind, EntityRef, SnapshotEntity};

pub const SNAPSHOT_CAPABILITY: &str = "snapshot.v1";
pub const WATCH_CAPABILITY: &str = "snapshot_watch.v1";
pub const HISTORY_CAPABILITY: &str = "snapshot_history.v1";
pub const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CHUNK_ENTITIES: usize = 256;
pub const MAX_SNAPSHOT_CHUNKS: u32 = 4096;
pub const MAX_HISTORICAL_RUNS: usize = 128;

pub use cache::{Snapshot, SnapshotAssembler, SynchronizedSnapshot};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EntityKey {
    pub kind: i32,
    pub id: String,
    pub agent_run_id: Option<String>,
}

impl EntityKey {
    pub fn from_entity(entity: &SnapshotEntity) -> Option<Self> {
        let (kind, id, agent_run_id) = match entity.body.as_ref()? {
            Body::Workspace(item) => (EntityKind::Workspace, &item.workspace_id, None),
            Body::Terminal(item) => (EntityKind::Terminal, &item.terminal_id, None),
            Body::Client(item) => (EntityKind::Client, &item.client_id, None),
            Body::Agent(item) => (
                EntityKind::Agent,
                &item.summary.as_ref()?.agent_run_id,
                None,
            ),
            Body::Request(item) => (
                EntityKind::Request,
                &item.request_id,
                Some(&item.agent_run_id),
            ),
            Body::Attention(item) => (
                EntityKind::Attention,
                &item.attention_id,
                Some(&item.agent_run_id),
            ),
        };
        Some(Self {
            kind: kind as i32,
            id: id.clone(),
            agent_run_id: agent_run_id.cloned(),
        })
    }

    pub fn from_reference(reference: &EntityRef) -> Self {
        Self {
            kind: reference.kind,
            id: reference.id.clone(),
            agent_run_id: reference.agent_run_id.clone(),
        }
    }

    pub fn reference(&self) -> EntityRef {
        EntityRef {
            kind: self.kind,
            id: self.id.clone(),
            agent_run_id: self.agent_run_id.clone(),
        }
    }
}
