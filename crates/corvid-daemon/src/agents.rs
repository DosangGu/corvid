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
use corvid_protocol::collector::CLAUDE_VERSION;
use corvid_protocol::wire::{
    AgentBadge, AgentCollection, AgentEndReason, AgentInfo, AgentList, AgentProcessState,
    AgentProvider, ProcessIdentity,
};
use uuid::Uuid;

use crate::process::{Executable, HookOwner, Incarnation};
use crate::projection;
use crate::publication::{Group, Projection};
use crate::workspaces::Registry;

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
    owner: Option<HookOwner>,
    sessions: BTreeSet<String>,
    context_deliveries: VecDeque<Uuid>,
    gap: bool,
    ambiguous: bool,
}

pub(crate) enum HookBinding {
    Starting,
    Owned(HookOwner),
    Unavailable,
}

struct ManagedRun {
    run: AgentRun,
    location: Location,
    process: Option<Incarnation>,
    qualified_version: Option<&'static str>,
    published: bool,
    terminal_available: bool,
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
                qualified_version: qualified.then_some(CLAUDE_VERSION),
                published: false,
                terminal_available: false,
            },
        );
        self.launches.insert(
            association,
            Launch {
                current: id,
                qualified,
                owner: None,
                sessions: BTreeSet::new(),
                context_deliveries: VecDeque::new(),
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
        self.retired.push_back(id);
        if self.retired.len() > RETAINED_RUNS
            && let Some(oldest) = self.retired.pop_front()
        {
            self.runs.remove(&oldest);
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

    pub(crate) fn tick(&mut self) {
        let monotonic = monotonic_ms();
        let wall = unix_ms();
        for launch in self.launches.values() {
            if let Some(record) = self.runs.get_mut(&launch.current)
                && record.run.tick(monotonic, wall)
            {
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

    pub(crate) fn focus_terminal(&self, run: Uuid) -> Option<Uuid> {
        let record = self.runs.get(&run)?;
        let identity = &record.run.identity;
        let launch = self.launches.get(&identity.launch_association)?;
        (launch.current == run
            && identity.parent_run_id.is_none()
            && record.published
            && record.terminal_available
            && !matches!(record.run.lifecycle.value, Lifecycle::Ended(_))
            && !matches!(record.run.process.value, ProcessLiveness::Exited { .. }))
        .then_some(identity.terminal_id)
    }

    pub(crate) fn binding(&self, association: Uuid) -> HookBinding {
        let Some(launch) = self.launches.get(&association) else {
            return HookBinding::Unavailable;
        };
        if let Some(owner) = launch.owner {
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
            terminal_available: self.terminal_available,
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
        executable: Option<Executable>,
    ) {
        self.with_registry(|registry| {
            let agents = &mut registry.agents;
            let Some(launch) = agents.launches.get_mut(&self.association) else {
                return;
            };
            launch.owner = incarnation
                .zip(executable)
                .map(|(incarnation, executable)| HookOwner {
                    incarnation,
                    executable,
                });
            let Some(record) = agents.runs.get_mut(&launch.current) else {
                return;
            };
            record.observe_process(liveness, incarnation, source);
            if launch.owner.is_none() {
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
            record.terminal_available = true;
            agents.dirty.insert(launch.current);
            let terminal = record.run.identity.terminal_id;
            let location = record.location;
            registry.terminal_started(terminal, location);
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
            for record in agents
                .runs
                .values_mut()
                .filter(|record| record.run.identity.launch_association == self.association)
            {
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
