mod children;
mod hooks;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Mutex, OnceLock, Weak};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use corvid_core::agent::{
    AgentRun, Badge, Collection, Confidence, EndReason, Evidence, Lifecycle, Observation,
    ObservationKind, ProcessLiveness, Provider, RequestStatus, RunIdentity, Source,
};
use corvid_core::layout::Location;
use corvid_protocol::wire::{
    AgentBadge, AgentCollection, AgentEndReason, AgentInfo, AgentList, AgentProcessState,
    AgentProvider, ProcessIdentity,
};
use uuid::Uuid;

use crate::process::{HookProducer, Incarnation, ProcessImage};
use crate::projection;
use crate::provider::qualified_version;
use crate::publication::{Group, Projection};
use crate::workspaces::Registry;
use children::{ChildEntry, ChildKey};

pub(crate) use hooks::Hook;

const RETAINED_RUNS: usize = 64;

#[derive(Default)]
pub(crate) struct Agents {
    runs: BTreeMap<Uuid, ManagedRun>,
    retired: VecDeque<Uuid>,
    launches: BTreeMap<Uuid, Launch>,
    dirty: BTreeSet<Uuid>,
}

struct Launch {
    current: Uuid,
    qualified: bool,
    owner: Option<ProcessImage>,
    producer: Option<HookProducer>,
    sessions: BTreeSet<String>,
    context_deliveries: VecDeque<Uuid>,
    resume: Option<PendingResume>,
    resume_turns: BTreeSet<String>,
    gap: bool,
    ambiguous: bool,
}

struct PendingResume {
    turn: String,
    deadline_ms: u64,
}

pub(crate) enum HookBinding {
    Starting,
    Owned(ProcessImage),
    Unavailable,
}

struct ManagedRun {
    run: AgentRun,
    location: Location,
    process: Option<Incarnation>,
    qualified_version: Option<&'static str>,
    published: bool,
    terminal_available: bool,
    children: BTreeMap<ChildKey, ChildEntry>,
    child_coverage_gap: bool,
}

impl Agents {
    fn reserve(
        &mut self,
        location: Location,
        terminal_id: Uuid,
        provider: Provider,
        qualified: bool,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let association = Uuid::new_v4();
        let identity = RunIdentity {
            id,
            terminal_id,
            launch_association: association,
            provider,
            provider_session: None,
            parent_run_id: None,
            actor: None,
        };
        self.runs.insert(
            id,
            ManagedRun {
                run: AgentRun::new(identity, evidence(Source::Launch, Confidence::Unknown)),
                location,
                process: None,
                qualified_version: qualified.then(|| qualified_version(provider)),
                published: false,
                terminal_available: false,
                children: BTreeMap::new(),
                child_coverage_gap: false,
            },
        );
        self.launches.insert(
            association,
            Launch {
                current: id,
                qualified,
                owner: None,
                producer: None,
                sessions: BTreeSet::new(),
                context_deliveries: VecDeque::new(),
                resume: None,
                resume_turns: BTreeSet::new(),
                gap: false,
                ambiguous: false,
            },
        );
        association
    }

    fn retire(&mut self, id: Uuid) {
        let Some(record) = self.runs.get_mut(&id) else {
            return;
        };
        if !record.published {
            self.runs.remove(&id);
            return;
        }
        record.terminal_available = false;
        self.dirty.insert(id);
        self.end_children(id);
        self.retired.push_back(id);
        if self.retired.len() > RETAINED_RUNS
            && let Some(oldest) = self.retired.pop_front()
        {
            if let Some(expired) = self.runs.remove(&oldest) {
                for child in expired.children.values().filter_map(ChildEntry::run) {
                    self.runs.remove(&child);
                    self.dirty.insert(child);
                }
            }
            self.dirty.insert(oldest);
        }
    }

    pub(crate) fn retire_launch(&mut self, association: Uuid) {
        if let Some(launch) = self.launches.remove(&association) {
            self.retire(launch.current);
        }
    }

    pub(crate) fn take_projections(&mut self) -> impl Iterator<Item = Projection> + '_ {
        std::mem::take(&mut self.dirty).into_iter().map(|id| {
            let entries = self
                .runs
                .get(&id)
                .filter(|record| record.published)
                .map_or_else(Vec::new, |record| {
                    projection::agent(&record.run, record.info())
                });
            (Group::Agent(id), entries)
        })
    }

    pub(crate) fn inventory(&self) -> AgentList {
        let agents = self
            .runs
            .values()
            .filter(|record| record.published)
            .map(ManagedRun::info)
            .collect();
        AgentList { agents }
    }

    pub(crate) fn relocate_terminal(&mut self, terminal: Uuid, location: Location) {
        for record in self.runs.values_mut() {
            if record.run.identity.terminal_id == terminal {
                record.location = location;
                self.dirty.insert(record.run.identity.id);
            }
        }
    }

    pub(crate) fn tick(&mut self) {
        let monotonic = monotonic_ms();
        let wall = unix_ms();
        for launch in self.launches.values_mut() {
            let Some(record) = self.runs.get_mut(&launch.current) else {
                continue;
            };
            let expired = launch.expire_resume(record, monotonic);
            if record.run.tick(monotonic, wall) || expired {
                self.dirty.insert(launch.current);
            }
        }
    }

    pub(crate) fn acknowledge(&mut self, run: Uuid, attention: Uuid) -> Option<u64> {
        let record = self.runs.get_mut(&run).filter(|record| record.published)?;
        if record.run.acknowledge(attention, unix_ms()) {
            self.dirty.insert(run);
        }
        record.run.attention.get(&attention)?.acknowledged_unix_ms
    }

    pub(crate) fn focus_terminal(&self, run: Uuid) -> Option<(Uuid, ProcessImage)> {
        let record = self.runs.get(&run)?;
        let identity = &record.run.identity;
        let launch = self.launches.get(&identity.launch_association)?;
        let owner = launch.owner?;
        (launch.current == run
            && identity.parent_run_id.is_none()
            && record.published
            && record.terminal_available
            && !matches!(record.run.lifecycle.value, Lifecycle::Ended(_))
            && record.run.process.value == ProcessLiveness::Running)
            .then_some((identity.terminal_id, owner))
    }

    pub(crate) fn binding(&self, association: Uuid) -> HookBinding {
        let Some(launch) = self.launches.get(&association) else {
            return HookBinding::Unavailable;
        };
        if launch.qualified
            && let Some(owner) = launch.owner
        {
            return HookBinding::Owned(owner);
        }
        if launch.qualified
            && self
                .runs
                .get(&launch.current)
                .is_some_and(|run| !run.published)
        {
            HookBinding::Starting
        } else {
            HookBinding::Unavailable
        }
    }
}

impl ManagedRun {
    fn info(&self) -> AgentInfo {
        let run = &self.run;
        let (process, exit_code) = match run.process.value {
            ProcessLiveness::Running => (AgentProcessState::Running, None),
            ProcessLiveness::Exited { code } => (AgentProcessState::Exited, code),
            ProcessLiveness::Unknown => (AgentProcessState::Unknown, None),
        };
        let provider = match run.identity.provider {
            Provider::Claude => AgentProvider::Claude,
            Provider::Codex => AgentProvider::Codex,
        };
        let (badge, pending_requests) = wire_badge(run.badge());
        let collection = match run.collection.value {
            Collection::NotRegistered => AgentCollection::NotRegistered,
            Collection::Healthy => AgentCollection::Healthy,
            Collection::Degraded => AgentCollection::Degraded,
            Collection::GapDetected => AgentCollection::GapDetected,
            Collection::Unsupported => AgentCollection::Unsupported,
        };
        let end_reason = match run.lifecycle.value {
            Lifecycle::Ended(EndReason::ProcessExited) => AgentEndReason::ProcessExited,
            Lifecycle::Ended(EndReason::ContextEnded) => AgentEndReason::ContextEnded,
            Lifecycle::Ended(EndReason::ContextReplaced) => AgentEndReason::ContextReplaced,
            _ => AgentEndReason::Unspecified,
        };
        AgentInfo {
            agent_run_id: run.identity.id.to_string(),
            terminal_id: run.identity.terminal_id.to_string(),
            location: Some(projection::selection(self.location)),
            provider: provider as i32,
            process: process as i32,
            process_identity: self.process.map(|identity| ProcessIdentity {
                pid: identity.pid,
                boot_id: identity.boot_id.to_string(),
                start_ticks: identity.start_ticks,
                foreground_group: identity.foreground_group,
            }),
            process_observed_unix_ms: run.process.evidence.observed_unix_ms,
            exit_code,
            badge: badge as i32,
            pending_requests,
            terminal_available: self.terminal_available && process == AgentProcessState::Running,
            provider_session_id: run.identity.provider_session.clone(),
            collection: collection as i32,
            unverified_requests: self.request_count(RequestStatus::Unverified),
            resolved_requests: self.request_count(RequestStatus::Resolved),
            response_observed_unix_ms: run
                .recent_response
                .as_ref()
                .map(|response| response.evidence.observed_unix_ms),
            end_reason: end_reason as i32,
            qualified_provider_version: self.qualified_version.map(str::to_owned),
            cancelled_requests: self.request_count(RequestStatus::Cancelled),
            adapter: Some(crate::provider::adapter(
                run.identity.provider,
                self.qualified_version.is_some(),
            )),
            parent_run_id: run.identity.parent_run_id.map(|id| id.to_string()),
            actor_id: run.identity.actor.clone(),
            child_coverage_gap: self.child_coverage_gap,
        }
    }

    fn request_count(&self, status: RequestStatus) -> u32 {
        self.run
            .requests
            .values()
            .filter(|request| request.state.value == status)
            .count() as u32
    }

    fn observe_process(
        &mut self,
        liveness: ProcessLiveness,
        identity: Option<Incarnation>,
        source: Source,
    ) -> bool {
        let previous = self.process;
        if let Some(identity) = identity {
            self.process = Some(identity);
        }
        if liveness != ProcessLiveness::Running
            && let Some(process) = &mut self.process
        {
            process.foreground_group = None;
        }
        let confidence = if liveness == ProcessLiveness::Unknown {
            Confidence::Unknown
        } else {
            Confidence::Confirmed
        };
        let changed = self.apply(
            Uuid::new_v4(),
            ObservationKind::Process(liveness),
            evidence(source, confidence),
        );
        changed || previous != self.process
    }

    fn apply(&mut self, delivery_id: Uuid, kind: ObservationKind, evidence: Evidence) -> bool {
        let observation = Observation {
            run_id: self.run.identity.id,
            delivery_id,
            evidence,
            kind,
        };
        self.run.apply(observation, monotonic_ms())
    }
}

fn wire_badge(badge: Badge) -> (AgentBadge, u32) {
    let badge = match badge {
        Badge::NeedsInput(count) => return (AgentBadge::NeedsInput, count as u32),
        Badge::Unknown => AgentBadge::Unknown,
        Badge::Ended => AgentBadge::Ended,
        Badge::Working => AgentBadge::Working,
        Badge::Ready => AgentBadge::Ready,
        Badge::ResponseObserved => AgentBadge::ResponseObserved,
        Badge::Interrupted => AgentBadge::Interrupted,
        Badge::Error => AgentBadge::Error,
    };
    (badge, 0)
}

fn evidence(source: Source, confidence: Confidence) -> Evidence {
    Evidence {
        source,
        source_version: env!("CARGO_PKG_VERSION").to_owned(),
        observed_unix_ms: unix_ms(),
        provider_unix_ms: None,
        provider_event_id: None,
        confidence,
        uncertainty: (confidence == Confidence::Unknown)
            .then(|| "process evidence unavailable".to_owned()),
        coverage_generation: 0,
    }
}

fn unix_ms() -> u64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

fn monotonic_ms() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    u64::try_from(ORIGIN.get_or_init(Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX)
}

pub(crate) struct ManagedLaunch {
    registry: Weak<Mutex<Registry>>,
    pub(crate) association: Uuid,
}

impl ManagedLaunch {
    pub(crate) fn reserve(
        registry: &Weak<Mutex<Registry>>,
        location: Location,
        terminal: Uuid,
        provider: Provider,
        qualified: bool,
    ) -> Result<Self> {
        let coordinator = registry
            .upgrade()
            .context("workspace coordinator is shutting down")?;
        let association = coordinator
            .lock()
            .expect("workspace registry lock poisoned")
            .agents
            .reserve(location, terminal, provider, qualified);
        Ok(Self {
            registry: registry.clone(),
            association,
        })
    }

    pub(crate) fn started(
        &self,
        liveness: ProcessLiveness,
        source: Source,
        incarnation: Option<Incarnation>,
        owner: Option<ProcessImage>,
    ) {
        self.with_registry(|registry| {
            let agents = &mut registry.agents;
            let Some(launch) = agents.launches.get_mut(&self.association) else {
                return;
            };
            launch.owner = owner;
            let Some(record) = agents.runs.get_mut(&launch.current) else {
                return;
            };
            record.observe_process(liveness, incarnation, source);
            if !launch.qualified || launch.owner.is_none() {
                let health = if launch.qualified {
                    Collection::Degraded
                } else {
                    Collection::Unsupported
                };
                let mut evidence = evidence(Source::Collector, Confidence::Confirmed);
                let reason = if launch.qualified {
                    "process ownership unavailable"
                } else {
                    "provider release has no qualified hooks"
                };
                evidence.uncertainty = Some(reason.to_owned());
                record.apply(
                    Uuid::new_v4(),
                    ObservationKind::Collection(health),
                    evidence,
                );
            }
            record.published = true;
            record.terminal_available = launch.owner.is_some();
            agents.dirty.insert(launch.current);
            let terminal = record.run.identity.terminal_id;
            let location = record.location;
            registry.terminal_started(terminal, location);
        });
    }

    pub(crate) fn image_changed(&self, identity: Incarnation) {
        self.with_registry(|registry| {
            let agents = &mut registry.agents;
            let Some(launch) = agents.launches.get_mut(&self.association) else {
                return;
            };
            launch.owner = None;
            let current_id = launch.current;
            let Some(current) = agents.runs.get_mut(&current_id) else {
                return;
            };
            current.terminal_available = false;
            current.observe_process(
                ProcessLiveness::Running,
                Some(identity),
                Source::ProcessProbe,
            );
            let mut observed = evidence(Source::ProcessProbe, Confidence::Confirmed);
            observed.uncertainty = Some("managed process changed its executable image".to_owned());
            current.apply(
                Uuid::new_v4(),
                ObservationKind::ContextEnded(EndReason::ContextReplaced),
                observed,
            );
            agents.dirty.insert(current_id);
            agents.end_children(current_id);
            registry.publish(Vec::new());
        });
    }

    pub(crate) fn observe(
        &self,
        liveness: ProcessLiveness,
        identity: Option<Incarnation>,
        source: Source,
    ) {
        self.with_registry(|registry| {
            let agents = &mut registry.agents;
            for record in agents.runs.values_mut().filter(|record| {
                record.run.identity.launch_association == self.association
                    && record.run.identity.parent_run_id.is_none()
            }) {
                if record.observe_process(liveness, identity, source) {
                    agents.dirty.insert(record.run.identity.id);
                }
            }
            registry.publish(Vec::new());
        });
    }

    fn with_registry(&self, apply: impl FnOnce(&mut Registry)) {
        if let Some(registry) = self.registry.upgrade() {
            apply(&mut registry.lock().expect("workspace registry lock poisoned"));
        }
    }
}

impl Drop for ManagedLaunch {
    fn drop(&mut self) {
        self.with_registry(|registry| {
            registry.agents.retire_launch(self.association);
            registry.publish(Vec::new());
        });
    }
}
