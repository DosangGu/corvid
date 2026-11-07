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
        self.clear_placement(run)?;
        self.order.retain(|key| *key != run);
        self.suppressed.insert(run);
        Ok(())
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
        self.retire(runs, removed)?;
        self.admit(runs)?;
        let available: Vec<_> = self
            .overflow()
            .filter(|run| runs.get(run).is_some_and(|run| run.navigable))
            .collect();
        let free: Vec<_> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.run.is_none() && !slot.pinned)
            .map(|(index, _)| index)
            .collect();
        for (index, run) in free.into_iter().zip(available) {
            self.assign(index, run)?;
        }
        Ok(())
    }

    fn retire(
        &mut self,
        runs: &BTreeMap<RunKey, RunObservation>,
        removed: &BTreeSet<RunKey>,
    ) -> Result<()> {
        let ended =
            |run: &RunKey| removed.contains(run) || runs.get(run).is_some_and(|run| run.ended);
        let retired: Vec<_> = self
            .order
            .iter()
            .chain(&self.suppressed)
            .copied()
            .filter(ended)
            .collect();
        for run in &retired {
            self.clear_placement(*run)?;
            if self.ended.len() == MAX_HISTORY {
                self.ended.pop_front();
            }
            self.ended.push_back(*run);
        }
        if !retired.is_empty() {
            self.order.retain(|run| !ended(run));
            self.suppressed.retain(|run| !ended(run));
        }
        Ok(())
    }

    fn admit(&mut self, runs: &BTreeMap<RunKey, RunObservation>) -> Result<()> {
        let tracked: BTreeSet<_> = self.order.iter().chain(&self.ended).copied().collect();
        let mut new: Vec<_> = runs
            .iter()
            .filter_map(|(key, run)| {
                (run.eligible
                    && run.navigable
                    && !run.ended
                    && !tracked.contains(key)
                    && !self.suppressed.contains(key))
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

    fn clear_placement(&mut self, run: RunKey) -> Result<()> {
        if let Some(slot) = self.slots.iter_mut().find(|slot| slot.run == Some(run)) {
            slot.revise()?;
            slot.run = None;
        }
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
