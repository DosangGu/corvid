use std::collections::BTreeMap;

use corvid_core::agent::{
    AgentRun, Collection, Confidence, EndReason, Evidence, Lifecycle, ObservationKind, Response,
    ResponseOutcome, Source, WorkKey,
};
use corvid_protocol::wire::HookEvent;
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

#[derive(Default)]
pub(super) struct ChildEntry {
    run: Option<Uuid>,
    started: Option<Evidence>,
    linked: bool,
    response: Option<(Evidence, u64)>,
    discarded: bool,
}

impl ChildEntry {
    pub(super) fn run(&self) -> Option<Uuid> {
        self.run
    }

    fn observe(&mut self, event: HookEvent, observed: Evidence) {
        match event {
            HookEvent::SubagentStarted => {
                self.started.get_or_insert(observed);
            }
            HookEvent::SubagentLinked => self.linked = true,
            HookEvent::SubagentResponseObserved => {
                let deadline = monotonic_ms().saturating_add(EARLY_RESPONSE_WINDOW_MS);
                self.response.get_or_insert((observed, deadline));
            }
            _ => unreachable!("only qualified child hooks reach this path"),
        }
    }
}

impl Agents {
    pub(super) fn observe_child(
        &mut self,
        parent_id: Uuid,
        key: ChildKey,
        event: HookEvent,
        observed: Evidence,
    ) {
        let capacity = self.runs.len() < MAX_AGENT_RECORDS - MAX_PTY_WORKERS - RETAINED_RUNS;
        let parent = self.runs.get_mut(&parent_id).expect("verified parent run");
        if parent.children.len() >= MAX_CHILD_KEYS_PER_RUN && !parent.children.contains_key(&key) {
            if !std::mem::replace(&mut parent.child_coverage_gap, true) {
                self.dirty.insert(parent_id);
            }
            return;
        }

        let entry = parent.children.entry(key.clone()).or_default();
        if entry.discarded {
            return;
        }
        if let Some(id) = entry.run {
            if event == HookEvent::SubagentResponseObserved
                && self
                    .runs
                    .get_mut(&id)
                    .expect("retained child run")
                    .finish_child_work(&key, observed)
            {
                self.dirty.insert(id);
            }
            return;
        }

        entry.observe(event, observed);
        if !entry.linked || entry.started.is_none() {
            return;
        }
        if !capacity {
            *entry = ChildEntry {
                discarded: true,
                ..Default::default()
            };
            if !std::mem::replace(&mut parent.child_coverage_gap, true) {
                self.dirty.insert(parent_id);
            }
            return;
        }

        let observed = entry.started.take().expect("confirmed child start");
        let response = entry.response.take();
        let child_id = Uuid::new_v4();
        entry.run = Some(child_id);
        let mut child = parent.child_record(child_id, &key, &observed);
        child.start_child_work(&key, observed, response);
        self.runs.insert(child_id, child);
        self.dirty.insert(child_id);
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
    ) {
        if response
            .as_ref()
            .is_some_and(|(_, deadline)| monotonic_ms() >= *deadline)
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
