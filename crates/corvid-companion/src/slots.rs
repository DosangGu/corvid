use std::collections::{BTreeMap, BTreeSet, VecDeque};

use corvid_protocol::ProtocolFault;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_SLOTS: u32 = 64;
const MAX_TRACKED: usize = 4096;
const MAX_HISTORY: usize = 256;

type Result<T> = std::result::Result<T, ProtocolFault>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct RunKey {
    pub host: Uuid,
    pub epoch: Uuid,
    pub run: Uuid,
}

impl RunKey {
    fn valid(self) -> bool {
        !self.host.is_nil() && !self.epoch.is_nil() && !self.run.is_nil()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RunObservation {
    pub created_sequence: Option<u64>,
    pub pane: Option<Uuid>,
    pub navigable: bool,
    pub eligible: bool,
    pub ended: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Slot {
    pub revision: u64,
    pub pinned: bool,
    pub run: Option<RunKey>,
}

impl Slot {
    fn revise(&mut self) -> Result<()> {
        self.revision = self.revision.checked_add(1).ok_or_else(limit)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SlotLayout {
    slots: Vec<Slot>,
    order: Vec<RunKey>,
    suppressed: BTreeSet<RunKey>,
    ended: VecDeque<RunKey>,
    #[serde(default)]
    owners: Vec<OwnedPane>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct OwnedPane {
    run: RunKey,
    pane: Uuid,
}

impl SlotLayout {
    pub fn new(count: u32) -> Result<Self> {
        if !(1..=MAX_SLOTS).contains(&count) {
            return Err(ProtocolFault::new(
                "InvalidMessage",
                "slot count is out of range",
            ));
        }
        Ok(Self {
            slots: vec![
                Slot {
                    revision: 1,
                    pinned: false,
                    run: None
                };
                count as usize
            ],
            order: Vec::new(),
            suppressed: BTreeSet::new(),
            ended: VecDeque::new(),
            owners: Vec::new(),
        })
    }

    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    pub fn overflow(&self) -> impl Iterator<Item = RunKey> + '_ {
        let assigned: BTreeSet<_> = self.slots.iter().filter_map(|slot| slot.run).collect();
        self.order
            .iter()
            .copied()
            .filter(move |run| !assigned.contains(run))
    }

    pub fn ended(&self) -> impl Iterator<Item = RunKey> + '_ {
        self.ended.iter().rev().copied()
    }

    pub fn suppressed(&self) -> impl Iterator<Item = RunKey> + '_ {
        self.suppressed.iter().copied()
    }

    pub(crate) fn closed_panes(&self, panes: &BTreeSet<Uuid>) -> BTreeSet<RunKey> {
        self.owners
            .iter()
            .filter(|owner| !panes.contains(&owner.pane))
            .map(|owner| owner.run)
            .collect()
    }

    pub fn exact_slot(&self, id: u32, revision: u64, run: Option<RunKey>) -> Result<&Slot> {
        id.checked_sub(1)
            .and_then(|index| self.slots.get(index as usize))
            .filter(|slot| slot.revision == revision && slot.run == run)
            .ok_or_else(stale)
    }

    pub fn pin(&mut self, id: u32, revision: u64, run: Option<RunKey>, pin: bool) -> Result<()> {
        self.exact_slot(id, revision, run)?;
        let slot = &mut self.slots[(id - 1) as usize];
        if slot.pinned != pin {
            slot.revise()?;
            slot.pinned = pin;
        }
        Ok(())
    }

    pub fn remove(&mut self, run: RunKey) -> Result<()> {
        if !self.order.contains(&run) {
            return Err(stale());
        }
        if self.suppressed.len() == MAX_TRACKED {
            return Err(limit());
        }
        self.order.retain(|key| *key != run);
        self.suppressed.insert(run);
        self.compact()
    }

    pub fn readd(&mut self, run: RunKey) -> Result<()> {
        if !self.suppressed.remove(&run) {
            return Err(stale());
        }
        Ok(())
    }

    pub fn place(&mut self, id: u32, revision: u64, run: RunKey) -> Result<()> {
        self.exact_slot(id, revision, None)?;
        if !self.overflow().any(|key| key == run) {
            return Err(stale());
        }
        self.assign(id as usize - 1, run)
    }

    pub fn reconcile(
        &mut self,
        runs: &BTreeMap<RunKey, RunObservation>,
        removed: &BTreeSet<RunKey>,
    ) -> Result<()> {
        self.retire(removed)?;
        self.admit(runs)?;
        for run in self.order.iter().chain(&self.suppressed) {
            if let Some(pane) = runs.get(run).and_then(|run| run.pane) {
                if let Some(owner) = self.owners.iter_mut().find(|owner| owner.run == *run) {
                    owner.pane = pane;
                } else {
                    self.owners.push(OwnedPane { run: *run, pane });
                }
            }
        }
        self.compact()
    }

    fn retire(&mut self, removed: &BTreeSet<RunKey>) -> Result<()> {
        let ended = |run: &RunKey| removed.contains(run);
        let retired: Vec<_> = self
            .order
            .iter()
            .chain(&self.suppressed)
            .copied()
            .filter(ended)
            .collect();
        for run in &retired {
            if self.ended.len() == MAX_HISTORY {
                self.ended.pop_front();
            }
            self.ended.push_back(*run);
        }
        if !retired.is_empty() {
            self.order.retain(|run| !ended(run));
            self.suppressed.retain(|run| !ended(run));
            self.owners.retain(|owner| !ended(&owner.run));
        }
        Ok(())
    }

    fn admit(&mut self, runs: &BTreeMap<RunKey, RunObservation>) -> Result<()> {
        let tracked: BTreeSet<_> = self.order.iter().chain(&self.ended).copied().collect();
        let mut new: Vec<_> = runs
            .iter()
            .filter_map(|(key, run)| {
                (run.eligible && !tracked.contains(key) && !self.suppressed.contains(key))
                    .then_some((run.created_sequence.unwrap_or(u64::MAX), *key))
            })
            .collect();
        if self.order.len() + new.len() > MAX_TRACKED {
            return Err(limit());
        }
        new.sort_unstable();
        self.order.extend(new.into_iter().map(|(_, key)| key));
        Ok(())
    }

    fn compact(&mut self) -> Result<()> {
        // Preserve the displayed order, then admit queued runs in arrival order.
        // A pin follows its agent; it cannot reserve a hole in the packed queue.
        let pins: BTreeMap<_, _> = self
            .slots
            .iter()
            .filter_map(|slot| slot.run.map(|run| (run, slot.pinned)))
            .collect();
        let mut order: Vec<_> = self
            .slots
            .iter()
            .filter_map(|slot| slot.run)
            .filter(|run| self.order.contains(run))
            .collect();
        let placed: BTreeSet<_> = order.iter().copied().collect();
        order.extend(
            self.order
                .iter()
                .copied()
                .filter(|run| !placed.contains(run)),
        );
        for (index, slot) in self.slots.iter_mut().enumerate() {
            let run = order.get(index).copied();
            let pinned = run.and_then(|run| pins.get(&run)).copied().unwrap_or(false);
            if slot.run != run || slot.pinned != pinned {
                slot.revise()?;
                slot.run = run;
                slot.pinned = pinned;
            }
        }
        self.order = order;
        Ok(())
    }

    fn assign(&mut self, index: usize, run: RunKey) -> Result<()> {
        let slot = &mut self.slots[index];
        slot.revise()?;
        slot.run = Some(run);
        Ok(())
    }

    pub(crate) fn validate(&self, count: u32, host: Uuid) -> bool {
        if self.slots.len() != count as usize
            || self.order.len() > MAX_TRACKED
            || self.suppressed.len() > MAX_TRACKED
            || self.ended.len() > MAX_HISTORY
            || self.owners.len() > MAX_TRACKED * 2
        {
            return false;
        }
        let order: BTreeSet<_> = self.order.iter().copied().collect();
        let ended: BTreeSet<_> = self.ended.iter().copied().collect();
        let mut placed = BTreeSet::new();
        order.len() == self.order.len()
            && ended.len() == self.ended.len()
            && order.is_disjoint(&ended)
            && order.is_disjoint(&self.suppressed)
            && ended.is_disjoint(&self.suppressed)
            && self.owners.iter().all(|owner| {
                !owner.pane.is_nil()
                    && (order.contains(&owner.run) || self.suppressed.contains(&owner.run))
            })
            && self
                .owners
                .iter()
                .map(|owner| owner.run)
                .collect::<BTreeSet<_>>()
                .len()
                == self.owners.len()
            && order
                .iter()
                .chain(&ended)
                .chain(&self.suppressed)
                .all(|run| run.valid() && run.host == host)
            && self.slots.iter().all(|slot| {
                slot.revision != 0
                    && slot
                        .run
                        .is_none_or(|key| order.contains(&key) && placed.insert(key))
            })
    }
}

fn stale() -> ProtocolFault {
    ProtocolFault::new("StaleBinding", "displayed slot or run has changed")
}

fn limit() -> ProtocolFault {
    ProtocolFault::new(
        "LimitExceeded",
        "slot revision or run history limit reached",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_pane_ownership_and_only_retires_deleted_layout_members() {
        let key = RunKey {
            host: Uuid::new_v4(),
            epoch: Uuid::new_v4(),
            run: Uuid::new_v4(),
        };
        let pane = Uuid::new_v4();
        let mut layout = SlotLayout::new(2).unwrap();
        let runs = BTreeMap::from([(
            key,
            RunObservation {
                pane: Some(pane),
                created_sequence: Some(1),
                eligible: true,
                navigable: true,
                ended: false,
            },
        )]);
        layout.reconcile(&runs, &BTreeSet::new()).unwrap();
        let mut restored: SlotLayout =
            serde_json::from_slice(&serde_json::to_vec(&layout).unwrap()).unwrap();
        // Even after agent history is pruned, a retained pane keeps the card.
        let removed = restored.closed_panes(&BTreeSet::from([pane]));
        restored.reconcile(&BTreeMap::new(), &removed).unwrap();
        assert_eq!(restored.slots()[0].run, Some(key));
        let removed = restored.closed_panes(&BTreeSet::new());
        restored.reconcile(&BTreeMap::new(), &removed).unwrap();
        assert!(restored.slots().iter().all(|slot| slot.run.is_none()));
        assert_eq!(restored.ended().collect::<Vec<_>>(), [key]);
    }
}
