use std::collections::{BTreeMap, BTreeSet};

use prost::Message;

use super::{EntityKey, MAX_CHUNK_ENTITIES, MAX_SNAPSHOT_BYTES, MAX_SNAPSHOT_CHUNKS, validate};
use crate::session::valid_id;
use crate::wire::envelope::Body;
use crate::wire::{EntityKind, Envelope, SnapshotChunk, SnapshotDelta, SnapshotEntity};
use crate::{CONTROL_LIMIT, ProtocolFault, Session};

type Result<T> = std::result::Result<T, ProtocolFault>;
pub(super) type Entries = BTreeMap<EntityKey, SnapshotEntity>;

pub struct Snapshot {
    pub(super) entries: Entries,
    sequence: u64,
    bytes: usize,
    capabilities: Vec<String>,
}

impl Snapshot {
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    pub fn entities(&self) -> impl Iterator<Item = &SnapshotEntity> {
        self.entries.values()
    }

    fn insert_chunk(&mut self, chunk: SnapshotChunk) -> Result<()> {
        for entity in chunk.entities {
            let key = validate::key(&entity)?;
            self.bytes += entity_size(&entity);
            if self.bytes > MAX_SNAPSHOT_BYTES || self.entries.insert(key, entity).is_some() {
                return Err(resync(
                    "snapshot has duplicate entries or exceeds its size bound",
                ));
            }
        }
        Ok(())
    }

    fn apply(&mut self, sequence: u64, delta: SnapshotDelta) -> Result<bool> {
        if sequence <= self.sequence {
            return Ok(false);
        }
        if self.sequence.checked_add(1) != Some(sequence) {
            return Err(resync("watch sequence is not contiguous"));
        }
        let edits = self.delta_edits(delta)?;
        let deep_validation = edits.iter().any(|(key, replacement)| {
            replacement.is_none()
                || matches!(
                    EntityKind::try_from(key.kind),
                    Ok(EntityKind::Workspace | EntityKind::Terminal | EntityKind::Client)
                )
        });
        let mut previous = Vec::with_capacity(edits.len());
        let mut size = self.bytes;
        for (key, replacement) in edits {
            let old = match replacement {
                Some(entity) => {
                    size += entity_size(&entity);
                    self.entries.insert(key.clone(), entity)
                }
                None => self.entries.remove(&key),
            };
            size -= old.as_ref().map_or(0, entity_size);
            previous.push((key, old));
        }

        let check = if size > MAX_SNAPSHOT_BYTES {
            Err(resync("watch cache exceeds its size bound"))
        } else if deep_validation {
            validate::all(&self.entries)
        } else {
            previous
                .iter()
                .try_for_each(|(key, _)| validate::entity(&self.entries, &self.entries[key]))
        };
        if let Err(fault) = check {
            for (key, old) in previous {
                if let Some(entity) = old {
                    self.entries.insert(key, entity);
                } else {
                    self.entries.remove(&key);
                }
            }
            return Err(fault);
        }
        self.sequence = sequence;
        self.bytes = size;
        Ok(true)
    }

    fn delta_edits(
        &self,
        delta: SnapshotDelta,
    ) -> Result<BTreeMap<EntityKey, Option<SnapshotEntity>>> {
        let count = delta.upsert.len() + delta.remove.len();
        if !(1..=1024).contains(&count) || delta.encoded_len() >= CONTROL_LIMIT {
            return Err(resync("invalid transaction size"));
        }
        let mut edits = BTreeMap::new();
        for entity in delta.upsert {
            let key = validate::key(&entity)?;
            if edits.insert(key, Some(entity)).is_some() {
                return Err(resync("duplicate transaction entry"));
            }
        }
        for reference in delta.remove {
            let key = EntityKey::from_reference(&reference);
            if !self.entries.contains_key(&key) || edits.insert(key, None).is_some() {
                return Err(resync("invalid transaction removal"));
            }
        }
        Ok(edits)
    }
}

pub struct SynchronizedSnapshot {
    session: Session,
    subscription: Option<String>,
    snapshot: Snapshot,
    valid: bool,
}

impl SynchronizedSnapshot {
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn subscription_id(&self) -> Option<&str> {
        self.subscription.as_deref()
    }

    pub fn is_valid(&self) -> bool {
        self.valid
    }

    /// Apply one unsolicited watch frame. On error, retain the last valid snapshot.
    pub fn apply(&mut self, frame: Envelope) -> Result<bool> {
        if !self.valid {
            return Err(resync("watch cache is invalid"));
        }
        let result = self.apply_frame(frame);
        if result.is_err() {
            self.valid = false;
        }
        result
    }

    fn apply_frame(&mut self, frame: Envelope) -> Result<bool> {
        self.session.validate_scope(&frame)?;
        check_event(self.subscription_id(), &frame)?;
        match frame.body {
            Some(Body::SnapshotDelta(delta)) => self
                .snapshot
                .apply(required_sequence(&frame.sequence)?, delta),
            Some(Body::Error(fault)) => Err(ProtocolFault::from_wire(&fault)),
            _ => Err(resync("unexpected watch frame")),
        }
    }
}

pub struct SnapshotAssembler {
    session: Session,
    request_id: String,
    watch: bool,
    phase: Phase,
}

enum Phase {
    Begin,
    Base(Base),
    CatchUp(SynchronizedSnapshot),
    Closed,
}

struct Base {
    snapshot: Snapshot,
    subscription: Option<String>,
    next_chunk: u32,
    chunks: u32,
    received_bytes: usize,
}

impl SnapshotAssembler {
    pub fn new(session: Session, request_id: String, watch: bool) -> Self {
        Self {
            session,
            request_id,
            watch,
            phase: Phase::Begin,
        }
    }

    /// Expose a cache only after SnapshotEnd and, for a watch, its verified watermark.
    pub fn push(&mut self, frame: Envelope) -> Result<Option<SynchronizedSnapshot>> {
        let phase = std::mem::replace(&mut self.phase, Phase::Closed);
        self.session.validate_scope(&frame)?;
        match phase {
            Phase::Begin => self.begin(frame),
            Phase::Base(base) => self.base(base, frame),
            Phase::CatchUp(cache) => self.catch_up(cache, frame),
            Phase::Closed => Err(resync("snapshot setup is closed")),
        }
    }

    fn begin(&mut self, frame: Envelope) -> Result<Option<SynchronizedSnapshot>> {
        self.check_reply(&frame)?;
        let begin = match frame.body {
            Some(Body::SnapshotBegin(begin)) => begin,
            Some(Body::Error(fault)) => return Err(ProtocolFault::from_wire(&fault)),
            _ => return Err(resync("expected snapshot begin")),
        };
        let subscription = frame.subscription_id;
        if self.watch != subscription.is_some() || !subscription.as_deref().is_none_or(valid_id) {
            return Err(resync("invalid snapshot subscription"));
        }
        if !(1..=MAX_SNAPSHOT_CHUNKS).contains(&begin.chunks)
            || !same_capabilities(&begin.capabilities, &self.session.capabilities)
        {
            return Err(resync("invalid snapshot declaration"));
        }
        let snapshot = Snapshot {
            entries: BTreeMap::new(),
            sequence: required_sequence(&frame.sequence)?,
            bytes: 0,
            capabilities: begin.capabilities,
        };
        self.phase = Phase::Base(Base {
            snapshot,
            subscription,
            next_chunk: 0,
            chunks: begin.chunks,
            received_bytes: 0,
        });
        Ok(None)
    }

    fn base(&mut self, mut base: Base, frame: Envelope) -> Result<Option<SynchronizedSnapshot>> {
        self.check_reply(&frame)?;
        if frame.subscription_id != base.subscription {
            return Err(resync("snapshot subscription changed"));
        }
        if let Some(Body::Error(fault)) = frame.body {
            return Err(ProtocolFault::from_wire(&fault));
        }
        if frame.sequence != Some(base.snapshot.sequence) {
            return Err(resync("snapshot base sequence changed"));
        }
        match frame.body {
            Some(Body::SnapshotChunk(chunk)) => {
                base.push(chunk)?;
                self.phase = Phase::Base(base);
                Ok(None)
            }
            Some(Body::SnapshotEnd(_)) if base.next_chunk == base.chunks => {
                validate::all(&base.snapshot.entries)?;
                let cache = SynchronizedSnapshot {
                    session: self.session.clone(),
                    subscription: base.subscription,
                    snapshot: base.snapshot,
                    valid: true,
                };
                if self.watch {
                    self.phase = Phase::CatchUp(cache);
                    Ok(None)
                } else {
                    Ok(Some(cache))
                }
            }
            _ => Err(resync("snapshot ended before its declared chunks arrived")),
        }
    }

    fn catch_up(
        &mut self,
        mut cache: SynchronizedSnapshot,
        frame: Envelope,
    ) -> Result<Option<SynchronizedSnapshot>> {
        if matches!(frame.body, Some(Body::WatchReady(_))) {
            check_event(cache.subscription_id(), &frame)?;
            if frame.sequence != Some(cache.snapshot.sequence) {
                return Err(resync("watch watermark has unapplied changes"));
            }
            return Ok(Some(cache));
        }
        if frame.request_id.as_deref() == Some(self.request_id.as_str())
            && frame.subscription_id.as_deref() == cache.subscription_id()
            && let Some(Body::Error(fault)) = &frame.body
        {
            return Err(ProtocolFault::from_wire(fault));
        }
        cache.apply(frame)?;
        self.phase = Phase::CatchUp(cache);
        Ok(None)
    }

    fn check_reply(&self, frame: &Envelope) -> Result<()> {
        if frame.request_id.as_deref() == Some(&self.request_id) {
            Ok(())
        } else {
            Err(resync("snapshot request ID changed"))
        }
    }
}

impl Base {
    fn push(&mut self, chunk: SnapshotChunk) -> Result<()> {
        let size = chunk.encoded_len();
        if chunk.index != self.next_chunk
            || self.next_chunk >= self.chunks
            || !(1..=MAX_CHUNK_ENTITIES).contains(&chunk.entities.len())
            || size >= CONTROL_LIMIT
            || self.received_bytes + size > MAX_SNAPSHOT_BYTES
        {
            return Err(resync("invalid snapshot chunk"));
        }
        self.snapshot.insert_chunk(chunk)?;
        self.next_chunk += 1;
        self.received_bytes += size;
        Ok(())
    }
}

fn check_event(subscription: Option<&str>, frame: &Envelope) -> Result<()> {
    if subscription.is_none()
        || frame.subscription_id.as_deref() != subscription
        || frame.request_id.is_some()
    {
        return Err(resync("invalid watch subscription scope"));
    }
    Ok(())
}

fn required_sequence(sequence: &Option<u64>) -> Result<u64> {
    sequence.ok_or_else(|| resync("missing publication sequence"))
}

fn same_capabilities(first: &[String], second: &[String]) -> bool {
    let offered: BTreeSet<_> = first.iter().collect();
    offered.len() == first.len()
        && first.len() == second.len()
        && offered == second.iter().collect()
}

fn entity_size(entity: &SnapshotEntity) -> usize {
    entity.encoded_len() + 6
}

pub(super) fn resync(message: &str) -> ProtocolFault {
    let mut fault = ProtocolFault::new("ResyncRequired", message);
    fault.retryable = true;
    fault
}
