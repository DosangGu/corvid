use std::collections::{BTreeMap, VecDeque};

use corvid_core::agent::{
    AgentRun, Collection, Confidence, EndReason, Evidence, Lifecycle, ObservationKind, Response,
    ResponseOutcome, Source, WorkKey,
};
use uuid::Uuid;

use super::{Agents, ManagedRun, RETAINED_RUNS, evidence, monotonic_ms};
use crate::workspaces::MAX_PTY_WORKERS;

const MAX_AGENT_RECORDS: usize = 1_024;
const MAX_CHILD_KEYS_PER_RUN: usize = 256;
const EARLY_RESPONSE_WINDOW_MS: u64 = 5_000;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ChildKey {
    pub(super) turn: String,
    pub(super) actor: String,
}

pub(super) enum ChildEvent {
    Started,
    Responded,
    Linked(ChildOwner),
}

#[derive(PartialEq, Eq)]
pub(super) enum ChildOwner {
    Root,
    Actor(String),
}

#[derive(Default)]
pub(super) struct ChildEntry {
    run: Option<Uuid>,
    started: Option<Evidence>,
    owner: Option<ChildOwner>,
    // The immediate link can match while its ancestors are still unpublished.
    matched_at: Option<u64>,
    response: Option<(Evidence, u64)>,
    discarded: bool,
}

impl ChildEntry {
    pub(super) fn run(&self) -> Option<Uuid> {
        self.run
    }

    fn observe(&mut self, event: ChildEvent, observed: Evidence) -> bool {
        match event {
            ChildEvent::Started => {
                self.started.get_or_insert(observed);
            }
            ChildEvent::Linked(owner) => {
                if self
                    .owner
                    .as_ref()
                    .is_some_and(|existing| existing != &owner)
                {
                    return false;
                }
                self.owner = Some(owner);
            }
            ChildEvent::Responded => {
                let deadline = monotonic_ms().saturating_add(EARLY_RESPONSE_WINDOW_MS);
                self.response.get_or_insert((observed, deadline));
            }
        }
        if self.started.is_some() && self.owner.is_some() {
            self.matched_at.get_or_insert_with(monotonic_ms);
        }
        true
    }
}

impl Agents {
    pub(super) fn observe_child(
        &mut self,
        root_id: Uuid,
        key: ChildKey,
        event: ChildEvent,
        observed: Evidence,
    ) {
        let root = self.runs.get_mut(&root_id).expect("verified root run");
        if root.children.len() >= MAX_CHILD_KEYS_PER_RUN && !root.children.contains_key(&key) {
            self.mark_child_coverage_gap(root_id);
            return;
        }

        let entry = root.children.entry(key.clone()).or_default();
        if entry.discarded {
            return;
        }
        if let Some(id) = entry.run {
            match event {
                ChildEvent::Responded => {
                    let child = self.runs.get_mut(&id).expect("retained child run");
                    if child.finish_child_work(&key, observed) {
                        self.dirty.insert(id);
                    }
                    return;
                }
                ChildEvent::Started => return,
                ChildEvent::Linked(_) => {}
            }
        }

        if !entry.observe(event, observed) {
            entry.discarded = true;
            let published = entry.run;
            self.mark_child_conflict(root_id, published);
        } else if entry.run.is_none() && entry.matched_at.is_some() {
            self.publish_children(root_id, key);
        }
    }

    fn publish_children(&mut self, root_id: Uuid, key: ChildKey) {
        let mut pending = VecDeque::from([key]);
        while let Some(key) = pending.pop_front() {
            if !self.publish_child(root_id, &key) {
                continue;
            }
            // A newly anchored parent can release descendants that arrived first.
            let children = self.runs[&root_id].children.iter().filter(|(child, entry)| {
                child.turn == key.turn
                    && matches!(&entry.owner, Some(ChildOwner::Actor(actor)) if actor == &key.actor)
            });
            pending.extend(children.map(|(child, _)| child.clone()));
        }
    }

    fn publish_child(&mut self, root_id: Uuid, key: &ChildKey) -> bool {
        let capacity = self.runs.len() < MAX_AGENT_RECORDS - MAX_PTY_WORKERS - RETAINED_RUNS;
        let root = self.runs.get_mut(&root_id).expect("verified root run");
        let entry = &root.children[key];
        let Some(matched_at) = entry
            .matched_at
            .filter(|_| entry.run.is_none() && !entry.discarded)
        else {
            return false;
        };
        let parent_id = match entry.owner.as_ref().expect("matched child owner") {
            ChildOwner::Root => Some(root_id),
            ChildOwner::Actor(actor) => root
                .children
                .get(&ChildKey {
                    turn: key.turn.clone(),
                    actor: actor.clone(),
                })
                .filter(|parent| !parent.discarded)
                .and_then(ChildEntry::run),
        };
        let Some(parent_id) = parent_id else {
            return false;
        };
        let entry = root.children.get_mut(key).expect("registered child key");
        if !capacity {
            entry.discarded = true;
            self.mark_child_coverage_gap(root_id);
            return false;
        }
        let observed = entry.started.take().expect("confirmed child start");
        let response = entry.response.take();
        let child_id = Uuid::new_v4();
        entry.run = Some(child_id);
        let parent = &self.runs[&parent_id];
        let mut child = parent.child_record(child_id, key, &observed);
        child.start_child_work(key, observed, response, matched_at);
        self.runs.insert(child_id, child);
        self.dirty.insert(child_id);
        true
    }

    fn mark_child_coverage_gap(&mut self, root_id: Uuid) {
        let root = self.runs.get_mut(&root_id).expect("verified root run");
        if !std::mem::replace(&mut root.child_coverage_gap, true) {
            self.dirty.insert(root_id);
        }
    }

    fn mark_child_conflict(&mut self, root_id: Uuid, child_id: Option<Uuid>) {
        self.mark_child_coverage_gap(root_id);
        if let Some(id) = child_id {
            let child = self.runs.get_mut(&id).expect("retained child run");
            let mut observed = evidence(Source::Collector, Confidence::Confirmed);
            observed.uncertainty = Some("conflicting child ownership links".to_owned());
            if child.apply(
                Uuid::new_v4(),
                ObservationKind::Collection(Collection::GapDetected),
                observed,
            ) {
                self.dirty.insert(id);
            }
        }
    }

    pub(super) fn end_children(&mut self, parent_id: Uuid) {
        let parent = self.runs.get(&parent_id).expect("retained parent run");
        let children: Vec<_> = parent
            .children
            .values()
            .filter_map(ChildEntry::run)
            .collect();
        for id in children {
            let child = self.runs.get_mut(&id).expect("retained child run");
            let observed = evidence(Source::Collector, Confidence::Confirmed);
            if child.apply(
                Uuid::new_v4(),
                ObservationKind::ContextEnded(EndReason::ContextEnded),
                observed,
            ) {
                self.dirty.insert(id);
            }
        }
    }
}

impl ChildKey {
    fn work(&self) -> WorkKey {
        WorkKey {
            turn: Some(self.turn.clone()),
            item: self.actor.clone(),
        }
    }
}

impl ManagedRun {
    fn child_record(&self, id: Uuid, key: &ChildKey, observed: &Evidence) -> Self {
        let mut identity = self.run.identity.clone();
        identity.parent_run_id = Some(identity.id);
        identity.id = id;
        identity.actor = Some(key.actor.clone());

        let mut initial = observed.clone();
        initial.confidence = Confidence::Unknown;
        let mut child = Self {
            run: AgentRun::new(identity, initial),
            location: self.location,
            process: None,
            qualified_version: self.qualified_version,
            published: true,
            terminal_available: false,
            children: BTreeMap::new(),
            child_coverage_gap: false,
        };
        child.apply(
            Uuid::new_v4(),
            ObservationKind::ContextObserved,
            observed.clone(),
        );
        child.apply(
            Uuid::new_v4(),
            ObservationKind::Collection(Collection::Healthy),
            observed.clone(),
        );
        child
    }

    fn start_child_work(
        &mut self,
        key: &ChildKey,
        mut started: Evidence,
        response: Option<(Evidence, u64)>,
        matched_at: u64,
    ) {
        if response
            .as_ref()
            .is_some_and(|(_, deadline)| matched_at >= *deadline)
        {
            started.uncertainty = Some(
                "child response arrived before ownership was confirmed and expired".to_owned(),
            );
            self.apply(
                Uuid::new_v4(),
                ObservationKind::Collection(Collection::GapDetected),
                started,
            );
            return;
        }

        self.apply(
            Uuid::new_v4(),
            ObservationKind::WorkStarted(key.work()),
            started,
        );
        if let Some((response, _)) = response {
            self.finish_child_work(key, response);
        }
    }

    fn finish_child_work(&mut self, key: &ChildKey, observed: Evidence) -> bool {
        if matches!(self.run.lifecycle.value, Lifecycle::Ended(_))
            || !self.apply(
                Uuid::new_v4(),
                ObservationKind::WorkFinished(key.work()),
                observed.clone(),
            )
        {
            return false;
        }
        self.apply(
            Uuid::new_v4(),
            ObservationKind::Response {
                response: Response {
                    turn: Some(key.turn.clone()),
                    outcome: ResponseOutcome::Observed,
                },
                final_boundary: false,
            },
            observed,
        );
        true
    }
}
