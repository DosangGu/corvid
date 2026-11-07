use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use corvid_protocol::snapshot::EntityKey;
use corvid_protocol::wire::snapshot_entity::Body;
use corvid_protocol::wire::{SnapshotDelta, SnapshotEntity};
use corvid_protocol::{CONTROL_LIMIT, ProtocolFault};
use prost::Message;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

const MAX_WATCHES: usize = 8;
const MAX_QUEUED_DELTAS: usize = 1024;
const MAX_QUEUED_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const BODY_BUDGET: usize = CONTROL_LIMIT - 1024;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Group {
    Layout,
    Terminal(Uuid),
    Client(Uuid),
    Agent(Uuid),
}

pub(crate) type Projection = (Group, Vec<SnapshotEntity>);
type Entities = BTreeMap<EntityKey, Arc<SnapshotEntity>>;

#[derive(Default)]
pub(crate) struct Publication {
    sequence: u64,
    exhausted: bool,
    groups: BTreeMap<Group, Entities>,
    watchers: Vec<Subscriber>,
}

pub(crate) struct Capture {
    pub(crate) sequence: u64,
    pub(crate) entities: Vec<Arc<SnapshotEntity>>,
    pub(crate) watch: Option<Watch>,
}

#[derive(Default)]
struct Changes {
    upserts: Vec<Arc<SnapshotEntity>>,
    removed: Vec<EntityKey>,
}

pub(crate) struct Delta {
    pub(crate) sequence: u64,
    changes: Changes,
    size: usize,
}

struct Subscriber {
    sender: mpsc::Sender<Queued>,
    overflow: watch::Sender<bool>,
    bytes: Arc<AtomicUsize>,
}

struct Queued {
    delta: Arc<Delta>,
    bytes: Arc<AtomicUsize>,
}

pub(crate) struct Watch {
    pub(crate) id: String,
    receiver: mpsc::Receiver<Queued>,
    overflow: watch::Receiver<bool>,
}

impl Publication {
    pub(crate) fn seed(layout: Projection) -> Self {
        let mut publication = Self::default();
        publication.replace(layout, &mut Changes::default());
        publication
    }

    pub(crate) fn sequence(&self) -> Result<u64, ProtocolFault> {
        if self.exhausted {
            return Err(ProtocolFault::new(
                "LimitExceeded",
                "publication sequence exhausted",
            ));
        }
        Ok(self.sequence)
    }

    /// Replace whole projections atomically. Unchanged entities retain their allocation.
    pub(crate) fn commit(&mut self, projections: Vec<Projection>) {
        let mut changes = Changes::default();
        for projection in projections {
            self.replace(projection, &mut changes);
        }
        if changes.upserts.is_empty() && changes.removed.is_empty() {
            return;
        }
        let Some(sequence) = self.sequence.checked_add(1) else {
            self.exhausted = true;
            self.disconnect_watchers();
            return;
        };
        self.sequence = sequence;
        self.watchers
            .retain(|subscriber| !subscriber.sender.is_closed());
        if self.watchers.is_empty() {
            return;
        }

        let size = changes.encoded_upper_bound();
        if size > BODY_BUDGET || changes.upserts.len() + changes.removed.len() > MAX_QUEUED_DELTAS {
            self.disconnect_watchers();
            return;
        }
        let delta = Arc::new(Delta {
            sequence,
            changes,
            size,
        });
        self.watchers
            .retain(|subscriber| subscriber.publish(&delta));
    }

    fn replace(&mut self, (group, entities): Projection, changes: &mut Changes) {
        let mut previous = self.groups.remove(&group).unwrap_or_default();
        let mut next = BTreeMap::new();
        for mut entity in entities {
            let key = EntityKey::from_entity(&entity).expect("published entity must have a key");
            let old = previous.remove(&key);
            if let Some(Body::Agent(agent)) = &mut entity.body {
                agent.created_sequence = old
                    .as_deref()
                    .and_then(|entity| match &entity.body {
                        Some(Body::Agent(agent)) => agent.created_sequence,
                        _ => None,
                    })
                    .or_else(|| self.sequence.checked_add(1));
            }
            let entry = match old {
                Some(old) if *old == entity => old,
                _ => {
                    let entity = Arc::new(entity);
                    changes.upserts.push(Arc::clone(&entity));
                    entity
                }
            };
            next.insert(key, entry);
        }
        changes.removed.extend(previous.into_keys());
        if !next.is_empty() {
            self.groups.insert(group, next);
        }
    }

    pub(crate) fn capture(&mut self, watch: bool) -> Result<Capture, ProtocolFault> {
        let sequence = self.sequence()?;
        let watch = if watch { Some(self.subscribe()?) } else { None };
        let entities = self
            .groups
            .values()
            .flat_map(|group| group.values().cloned())
            .collect();
        Ok(Capture {
            sequence,
            entities,
            watch,
        })
    }

    fn subscribe(&mut self) -> Result<Watch, ProtocolFault> {
        self.watchers
            .retain(|subscriber| !subscriber.sender.is_closed());
        if self.watchers.len() >= MAX_WATCHES {
            return Err(ProtocolFault::new(
                "LimitExceeded",
                "too many active snapshot watches",
            ));
        }
        let (sender, receiver) = mpsc::channel(MAX_QUEUED_DELTAS);
        let (overflow, signal) = watch::channel(false);
        self.watchers.push(Subscriber {
            sender,
            overflow,
            bytes: Arc::new(AtomicUsize::new(0)),
        });
        Ok(Watch {
            id: Uuid::new_v4().to_string(),
            receiver,
            overflow: signal,
        })
    }

    fn disconnect_watchers(&mut self) {
        for subscriber in self.watchers.drain(..) {
            subscriber.overflow.send_replace(true);
        }
    }
}

impl Changes {
    fn encoded_upper_bound(&self) -> usize {
        let upserts: usize = self
            .upserts
            .iter()
            .map(|entity| entity.encoded_len() + 6)
            .sum();
        let removals: usize = self
            .removed
            .iter()
            .map(|key| key.reference().encoded_len() + 6)
            .sum();
        upserts + removals
    }
}

impl Delta {
    pub(crate) fn body(&self) -> SnapshotDelta {
        SnapshotDelta {
            upsert: self
                .changes
                .upserts
                .iter()
                .map(|entity| (**entity).clone())
                .collect(),
            remove: self
                .changes
                .removed
                .iter()
                .map(EntityKey::reference)
                .collect(),
        }
    }
}

impl Subscriber {
    fn publish(&self, delta: &Arc<Delta>) -> bool {
        let prior = self.bytes.fetch_add(delta.size, Ordering::Relaxed);
        let queued = Queued {
            delta: Arc::clone(delta),
            bytes: Arc::clone(&self.bytes),
        };
        if prior + delta.size > MAX_QUEUED_BYTES || self.sender.try_send(queued).is_err() {
            self.overflow.send_replace(true);
            return false;
        }
        true
    }
}

impl Drop for Queued {
    fn drop(&mut self) {
        self.bytes.fetch_sub(self.delta.size, Ordering::Relaxed);
    }
}

impl Watch {
    pub(crate) fn check(&self) -> Result<(), ProtocolFault> {
        if *self.overflow.borrow() {
            Err(resync())
        } else {
            Ok(())
        }
    }

    pub(crate) async fn next(&mut self) -> Result<Arc<Delta>, ProtocolFault> {
        self.check()?;
        tokio::select! {
            biased;
            _ = self.overflow.changed() => Err(resync()),
            queued = self.receiver.recv() => {
                let queued = queued.ok_or_else(resync)?;
                self.check()?;
                Ok(Arc::clone(&queued.delta))
            }
        }
    }
}

pub(crate) fn resync() -> ProtocolFault {
    let mut fault = ProtocolFault::new("ResyncRequired", "snapshot watch backlog is unavailable");
    fault.retryable = true;
    fault
}

#[cfg(test)]
mod tests {
    use corvid_protocol::wire::{AgentInfo, AgentState, WorkspaceState};

    use super::*;

    fn layout(label: String) -> Projection {
        let entity = SnapshotEntity {
            body: Some(Body::Workspace(WorkspaceState {
                workspace_id: "work".to_owned(),
                label,
                tabs: Vec::new(),
            })),
        };
        (Group::Layout, vec![entity])
    }

    #[tokio::test]
    async fn capturing_and_subscribing_share_the_same_barrier() {
        let mut state = Publication::seed(layout("before".to_owned()));
        let mut capture = state.capture(true).unwrap();
        state.commit(vec![layout("after".to_owned())]);
        state.commit(vec![layout("after".to_owned())]);

        assert_eq!(capture.sequence, 0);
        assert_eq!(*capture.entities[0], layout("before".to_owned()).1[0]);
        let delta = capture.watch.as_mut().unwrap().next().await.unwrap();
        assert_eq!(delta.sequence, 1);
        assert_eq!(delta.body().upsert, layout("after".to_owned()).1);
        assert_eq!(state.sequence().unwrap(), 1);
    }

    #[test]
    fn agent_creation_sequence_survives_later_publications() {
        let mut state = Publication::seed(layout("before".to_owned()));
        let run = Uuid::new_v4();
        let agent = |active_turns| {
            (
                Group::Agent(run),
                vec![SnapshotEntity {
                    body: Some(Body::Agent(Box::new(AgentState {
                        summary: Some(AgentInfo {
                            agent_run_id: run.to_string(),
                            ..Default::default()
                        }),
                        active_turns,
                        ..Default::default()
                    }))),
                }],
            )
        };
        state.commit(vec![layout("after".to_owned())]);
        state.commit(vec![agent(0)]);
        state.commit(vec![agent(1)]);
        state.commit(vec![agent(1)]);

        let capture = state.capture(false).unwrap();
        assert_eq!(capture.sequence, 3);
        let created = capture
            .entities
            .iter()
            .find_map(|entity| match &entity.body {
                Some(Body::Agent(agent)) => agent.created_sequence,
                _ => None,
            });
        assert_eq!(created, Some(2));
    }

    #[tokio::test]
    async fn slow_watches_disconnect_when_their_byte_budget_fills() {
        let mut state = Publication::seed(layout("before".to_owned()));
        let mut slow = state.capture(true).unwrap().watch.unwrap();
        let mut fast = state.capture(true).unwrap().watch.unwrap();
        for index in 1..=128 {
            let label = format!("{index:04}{}", "x".repeat(64 * 1024));
            state.commit(vec![layout(label)]);
            assert_eq!(fast.next().await.unwrap().sequence, index);
        }

        let Err(fault) = slow.next().await else {
            panic!("slow watch should require resync");
        };
        assert_eq!(fault.code, "ResyncRequired");
        let recovered = state.capture(true).unwrap();
        assert_eq!(recovered.sequence, 128);
    }

    #[test]
    fn exhausted_sequence_never_wraps_or_returns_a_stale_snapshot() {
        let mut state = Publication::seed(layout("before".to_owned()));
        state.sequence = u64::MAX - 1;
        state.commit(vec![layout("last".to_owned())]);
        assert_eq!(state.sequence().unwrap(), u64::MAX);
        state.commit(vec![layout("overflow".to_owned())]);

        let Err(fault) = state.capture(false) else {
            panic!("exhausted publication should reject snapshots");
        };
        assert_eq!(fault.code, "LimitExceeded");
    }
}
