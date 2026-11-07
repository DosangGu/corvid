use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use corvid_core::agent::{
    AgentRun, Badge, Confidence, Evidence, Observation, ObservationKind, ProcessLiveness, Provider,
    RunIdentity, Source,
};
use corvid_core::layout::Location;
use corvid_protocol::wire::{
    AgentBadge, AgentInfo, AgentList, AgentProcessState, AgentProvider, ProcessIdentity,
    ViewSelection,
};
use uuid::Uuid;

use crate::process::Incarnation;
use crate::workspaces::Registry;

const RETAINED_RUNS: usize = 64;

#[derive(Default)]
pub(crate) struct Agents {
    runs: BTreeMap<Uuid, ManagedRun>,
    retired: VecDeque<Uuid>,
}

struct ManagedRun {
    run: AgentRun,
    location: Location,
    process: Option<Incarnation>,
    published: bool,
    terminal_available: bool,
}

impl Agents {
    fn reserve(&mut self, location: Location, terminal_id: Uuid, provider: Provider) -> Uuid {
        let id = Uuid::new_v4();
        let identity = RunIdentity {
            id,
            terminal_id,
            launch_association: Uuid::new_v4(),
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
                published: false,
                terminal_available: false,
            },
        );
        id
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
        self.retired.push_back(id);
        if self.retired.len() > RETAINED_RUNS
            && let Some(oldest) = self.retired.pop_front()
        {
            self.runs.remove(&oldest);
        }
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
        AgentInfo {
            agent_run_id: run.identity.id.to_string(),
            terminal_id: run.identity.terminal_id.to_string(),
            location: Some(ViewSelection {
                workspace_id: self.location.workspace.to_string(),
                tab_id: self.location.tab.to_string(),
                pane_id: self.location.pane.to_string(),
            }),
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
        }
    }

    fn observe_process(
        &mut self,
        liveness: ProcessLiveness,
        identity: Option<Incarnation>,
        source: Source,
    ) {
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
        let observation = Observation {
            run_id: self.run.identity.id,
            delivery_id: Uuid::new_v4(),
            evidence: evidence(source, confidence),
            kind: ObservationKind::Process(liveness),
        };
        // Process observations do not participate in the orphan correlation timer.
        self.run.apply(observation, 0);
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
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Evidence {
        source,
        source_version: env!("CARGO_PKG_VERSION").to_owned(),
        observed_unix_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        provider_unix_ms: None,
        provider_event_id: None,
        confidence,
        uncertainty: (confidence == Confidence::Unknown)
            .then(|| "process evidence unavailable".to_owned()),
        coverage_generation: 0,
    }
}

pub(crate) struct ManagedLaunch {
    registry: Weak<Mutex<Registry>>,
    run: Uuid,
}

impl ManagedLaunch {
    pub(crate) fn reserve(
        registry: &Weak<Mutex<Registry>>,
        location: Location,
        terminal: Uuid,
        provider: Provider,
    ) -> Result<Self> {
        let coordinator = registry
            .upgrade()
            .context("workspace coordinator is shutting down")?;
        let run = coordinator
            .lock()
            .expect("workspace registry lock poisoned")
            .agents
            .reserve(location, terminal, provider);
        Ok(Self {
            registry: registry.clone(),
            run,
        })
    }

    pub(crate) fn started(&self, liveness: ProcessLiveness, source: Source) {
        self.with_run(|record| {
            record.observe_process(liveness, None, source);
            record.published = true;
            record.terminal_available = true;
        });
    }

    pub(crate) fn observe(
        &self,
        liveness: ProcessLiveness,
        identity: Option<Incarnation>,
        source: Source,
    ) {
        self.with_run(|record| record.observe_process(liveness, identity, source));
    }

    fn with_run(&self, apply: impl FnOnce(&mut ManagedRun)) {
        if let Some(registry) = self.registry.upgrade()
            && let Some(record) = registry
                .lock()
                .expect("workspace registry lock poisoned")
                .agents
                .runs
                .get_mut(&self.run)
        {
            apply(record);
        }
    }
}

impl Drop for ManagedLaunch {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            registry
                .lock()
                .expect("workspace registry lock poisoned")
                .agents
                .retire(self.run);
        }
    }
}
