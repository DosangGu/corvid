use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use corvid_core::agent::{
    AgentRun, Confidence, Evidence, Observation, ObservationKind, ProcessLiveness, Provider,
    RunIdentity, Source,
};
use corvid_core::layout::Location;
use corvid_protocol::wire::{
    AgentBadge, AgentCollection, AgentInfo, AgentList, ProviderAdapter, ProviderExecutionMode,
};
use uuid::Uuid;

use crate::native_process::{Found, Process, ProcessKey};
use crate::projection;
use crate::publication::{Group, Projection};

const RETAINED_RUNS: usize = 64;
const MAX_SHELL_RUNS: usize = 64;

#[derive(Default)]
pub(crate) struct Agents {
    runs: BTreeMap<Uuid, Run>,
    active: BTreeMap<Uuid, Uuid>,
    roots: BTreeMap<ProcessKey, Uuid>,
    shell: BTreeMap<ProcessKey, (Uuid, Process)>,
    retired: VecDeque<Uuid>,
    dirty: BTreeSet<Uuid>,
}

struct Run {
    state: AgentRun,
    location: Location,
    available: bool,
}

impl Agents {
    pub(crate) fn track_shell(&mut self, terminal: Uuid, root: ProcessKey) {
        self.roots.insert(root, terminal);
    }

    pub(crate) fn roots(&self) -> Vec<ProcessKey> {
        self.roots.keys().copied().collect()
    }

    pub(crate) fn sample_shells(
        &mut self,
        found: Vec<Found>,
        terminals: &std::collections::HashMap<Uuid, Location>,
    ) -> bool {
        let changed: Vec<_> = self
            .shell
            .iter()
            .filter_map(|(&key, (id, process))| {
                let liveness = process.liveness();
                (self.runs[id].state.process.value != liveness).then_some((key, *id, liveness))
            })
            .collect();
        for (key, id, liveness) in changed {
            self.observe_run(id, liveness);
            if matches!(liveness, ProcessLiveness::Exited { .. }) {
                self.shell.remove(&key);
                self.retire(id);
            }
        }
        for candidate in found {
            if self.shell.len() == MAX_SHELL_RUNS {
                break;
            }
            let key = candidate.process.key;
            let Some(&terminal) = self.roots.get(&candidate.root) else {
                continue;
            };
            let Some(&location) = terminals.get(&terminal) else {
                continue;
            };
            if self.shell.contains_key(&key)
                || candidate.process.liveness() != ProcessLiveness::Running
            {
                continue;
            }
            let id = self.start_run(
                terminal,
                location,
                candidate.provider,
                ProcessLiveness::Running,
            );
            self.shell.insert(key, (id, candidate.process));
        }
        !self.dirty.is_empty()
    }

    pub(crate) fn started(
        &mut self,
        terminal: Uuid,
        location: Location,
        provider: Provider,
        liveness: ProcessLiveness,
    ) {
        let id = self.start_run(terminal, location, provider, liveness);
        self.active.insert(terminal, id);
    }

    fn start_run(
        &mut self,
        terminal: Uuid,
        location: Location,
        provider: Provider,
        liveness: ProcessLiveness,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let identity = RunIdentity {
            id,
            terminal_id: terminal,
            launch_association: Uuid::new_v4(),
            provider,
            provider_session: None,
            parent_run_id: None,
            actor: None,
        };
        let state = AgentRun::new(identity, evidence(Source::Launch, Confidence::Unknown));
        self.runs.insert(
            id,
            Run {
                state,
                location,
                available: true,
            },
        );
        self.dirty.insert(id);
        self.observe_run(id, liveness);
        id
    }

    pub(crate) fn observe(&mut self, terminal: Uuid, liveness: ProcessLiveness) {
        let Some(&id) = self.active.get(&terminal) else {
            return;
        };
        self.observe_run(id, liveness);
    }

    fn observe_run(&mut self, id: Uuid, liveness: ProcessLiveness) {
        let (source, confidence) = match liveness {
            ProcessLiveness::Running => (Source::ProcessProbe, Confidence::Confirmed),
            ProcessLiveness::Exited { .. } => (Source::ChildExit, Confidence::Confirmed),
            ProcessLiveness::Unknown => (Source::ProcessProbe, Confidence::Unknown),
        };
        let run = self.runs.get_mut(&id).expect("active run");
        if run.state.apply(
            Observation {
                run_id: id,
                delivery_id: Uuid::new_v4(),
                evidence: evidence(source, confidence),
                kind: ObservationKind::Process(liveness),
            },
            0,
        ) {
            self.dirty.insert(id);
        }
    }

    pub(crate) fn terminal_unavailable(&mut self, terminal: Uuid) {
        self.roots.retain(|_, owner| *owner != terminal);
        for (&id, run) in &mut self.runs {
            if run.state.identity.terminal_id == terminal && run.available {
                run.available = false;
                self.dirty.insert(id);
            }
        }
    }

    pub(crate) fn retire_terminal(&mut self, terminal: Uuid) {
        self.terminal_unavailable(terminal);
        if let Some(id) = self.active.remove(&terminal) {
            self.retire(id);
        }
        let shells: Vec<_> = self
            .shell
            .iter()
            .filter(|(_, (id, _))| self.runs[id].state.identity.terminal_id == terminal)
            .map(|(&key, (id, process))| (key, *id, process.liveness()))
            .collect();
        for (key, id, liveness) in shells {
            self.shell.remove(&key);
            let liveness = if liveness == ProcessLiveness::Running {
                ProcessLiveness::Unknown
            } else {
                liveness
            };
            self.observe_run(id, liveness);
            self.retire(id);
        }
    }

    fn retire(&mut self, id: Uuid) {
        self.runs.get_mut(&id).expect("tracked run").available = false;
        self.dirty.insert(id);
        self.retired.push_back(id);
        if self.retired.len() > RETAINED_RUNS
            && let Some(expired) = self.retired.pop_front()
        {
            self.runs.remove(&expired);
            self.dirty.insert(expired);
        }
    }

    pub(crate) fn relocate_terminal(&mut self, terminal: Uuid, location: Location) {
        for (&id, run) in &mut self.runs {
            if run.state.identity.terminal_id == terminal {
                run.location = location;
                self.dirty.insert(id);
            }
        }
    }

    pub(crate) fn focus_terminal(&self, id: Uuid) -> Option<Uuid> {
        let run = self.runs.get(&id)?;
        run.live().then_some(run.state.identity.terminal_id)
    }

    pub(crate) fn take_projections(&mut self) -> impl Iterator<Item = Projection> + '_ {
        std::mem::take(&mut self.dirty).into_iter().map(|id| {
            let entries = self
                .runs
                .get(&id)
                .map_or_else(Vec::new, |run| projection::agent(&run.state, run.info()));
            (Group::Agent(id), entries)
        })
    }

    pub(crate) fn inventory(&self) -> AgentList {
        AgentList {
            agents: self.runs.values().map(Run::info).collect(),
        }
    }
}

impl Run {
    fn live(&self) -> bool {
        self.available && self.state.process.value == ProcessLiveness::Running
    }

    fn info(&self) -> AgentInfo {
        let run = &self.state;
        let (process, exit_code) = projection::process(run.process.value);
        let badge =
            if exit_code.is_some() || matches!(run.process.value, ProcessLiveness::Exited { .. }) {
                AgentBadge::Ended
            } else {
                AgentBadge::Unknown
            };
        AgentInfo {
            agent_run_id: run.identity.id.to_string(),
            terminal_id: run.identity.terminal_id.to_string(),
            location: Some(projection::selection(self.location)),
            provider: projection::provider(run.identity.provider) as i32,
            process: process as i32,
            process_observed_unix_ms: run.process.evidence.observed_unix_ms,
            exit_code,
            badge: badge as i32,
            terminal_available: self.live(),
            collection: AgentCollection::NotRegistered as i32,
            end_reason: projection::end_reason(run.lifecycle.value) as i32,
            adapter: Some(ProviderAdapter {
                mode: ProviderExecutionMode::ProcessOnly as i32,
                schema_revision: 1,
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

fn evidence(source: Source, confidence: Confidence) -> Evidence {
    let observed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Evidence {
        source,
        source_version: env!("CARGO_PKG_VERSION").into(),
        observed_unix_ms: observed.as_millis().try_into().unwrap_or(u64::MAX),
        provider_unix_ms: None,
        provider_event_id: None,
        confidence,
        uncertainty: (confidence == Confidence::Unknown)
            .then(|| "process evidence unavailable".into()),
        coverage_generation: 0,
    }
}
