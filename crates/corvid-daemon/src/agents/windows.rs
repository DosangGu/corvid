use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use corvid_async::watch;
use corvid_core::agent::{
    AgentRun, Badge, Collection, Confidence, EndReason, Evidence, Lifecycle, Observation,
    ObservationKind, ProcessLiveness, Provider, RequestStatus, RunIdentity, Source,
};
use corvid_core::layout::Location;
use corvid_protocol::ProtocolFault;
use corvid_protocol::wire::{
    AgentBadge, AgentCollection, AgentInfo, AgentList, HookEvent, ProviderAdapter,
    ProviderExecutionMode,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::handoff::RetainedCoverage;
use crate::handoff::input::{RetainedInput, valid_id};
use crate::hook_data::Hook;
use crate::native_process::{Found, HookOwner, Process, ProcessKey};
use crate::projection;
use crate::publication::{Group, Projection};

const RETAINED_RUNS: usize = 64;
const MAX_SHELL_RUNS: usize = 64;
const MAX_CONTEXTS: usize = 64;

pub(crate) enum HookBinding {
    Starting(watch::Receiver<Option<ProcessKey>>),
    Ready(ProcessKey),
}

impl HookBinding {
    pub(crate) async fn root(self) -> Option<ProcessKey> {
        match self {
            Self::Starting(mut start) => *start.wait_for(Option::is_some).await.ok()?,
            Self::Ready(root) => Some(root),
        }
    }
}

#[derive(Default)]
pub(crate) struct Agents {
    runs: BTreeMap<Uuid, Run>,
    active: BTreeMap<Uuid, Uuid>,
    roots: BTreeMap<ProcessKey, Uuid>,
    managed: BTreeMap<Uuid, ProcessKey>,
    starting: BTreeMap<Uuid, watch::Sender<Option<ProcessKey>>>,
    shell: BTreeMap<ProcessKey, (Uuid, Process)>,
    retained_observers: BTreeMap<Uuid, (ProcessKey, Uuid)>,
    retired: VecDeque<Uuid>,
    dirty: BTreeSet<Uuid>,
}

struct Run {
    state: AgentRun,
    location: Location,
    available: bool,
    producer: Option<ProcessKey>,
    sessions: BTreeSet<String>,
    retained_collection: Option<RetainedCoverage>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct RetainedContext {
    owner: ProcessKey,
    producer: ProcessKey,
    provider: Provider,
    session: String,
    sessions: BTreeSet<String>,
    turn: Option<String>,
    inputs: Vec<RetainedInput>,
    gap: bool,
    #[serde(default)]
    delivery_gap: bool,
}

impl RetainedContext {
    pub(crate) fn valid_all(contexts: &[Self]) -> bool {
        let mut owners = BTreeSet::new();
        contexts.len() <= MAX_SHELL_RUNS
            && contexts.iter().all(|context| {
                context.owner.valid()
                    && context.producer.valid()
                    && owners.insert(context.owner)
                    && valid_id(&context.session)
                    && context.sessions.len() <= MAX_CONTEXTS
                    && context.sessions.contains(&context.session)
                    && context.sessions.iter().all(|id| valid_id(id))
                    && context.turn.as_deref().is_none_or(valid_id)
                    && RetainedInput::valid_all(context.turn.as_deref(), &context.inputs)
            })
    }
}

impl Agents {
    pub(crate) fn observer_check(&self, terminal: Uuid) -> Option<crate::collector::ObserverCheck> {
        let managed = self.active.get(&terminal).zip(self.managed.get(&terminal));
        managed
            .into_iter()
            .chain(self.shell.iter().map(|(owner, (id, _))| (id, owner)))
            .find_map(|(&id, &owner)| {
                let run = self.runs.get(&id)?;
                if run.state.identity.terminal_id != terminal
                    || !run.live()
                    || run.producer.is_none()
                    || !(run.state.has_hook_delivery_gap()
                        || matches!(run.retained_collection, Some(RetainedCoverage::DeliveryGap)))
                {
                    return None;
                }
                Some(crate::collector::ObserverCheck {
                    run: id,
                    owner,
                    provider: run.state.identity.provider,
                })
            })
    }

    pub(crate) fn pending_input_panes(&self) -> BTreeSet<Uuid> {
        self.runs
            .values()
            .filter(|run| {
                run.live() && matches!(run.state.badge(), corvid_core::agent::Badge::NeedsInput(_))
            })
            .map(|run| run.location.pane)
            .collect()
    }

    pub(crate) fn resume_observations(
        &self,
    ) -> Vec<(Location, Option<corvid_core::agent::AgentSessionKey>)> {
        self.active
            .values()
            .copied()
            .chain(self.shell.values().map(|(id, _)| *id))
            .filter(|id| self.dirty.contains(id))
            .filter_map(|id| {
                let run = self.runs.get(&id)?;
                (run.producer.is_some()
                    && run.retained_collection.is_none()
                    && run.state.process.value == ProcessLiveness::Running)
                    .then(|| {
                        (
                            run.location,
                            self.confirmed_session(run.state.identity.terminal_id),
                        )
                    })
            })
            .collect()
    }

    pub(crate) fn confirmed_session(
        &self,
        terminal: Uuid,
    ) -> Option<corvid_core::agent::AgentSessionKey> {
        self.active
            .values()
            .copied()
            .chain(self.shell.values().map(|(id, _)| *id))
            .find_map(|id| {
                let run = self.runs.get(&id)?;
                let session = run.state.identity.provider_session.clone()?;
                (run.state.identity.terminal_id == terminal
                    && run.producer.is_some()
                    && run.state.process.value == ProcessLiveness::Running
                    && run.state.collection.value == Collection::Healthy)
                    .then_some(corvid_core::agent::AgentSessionKey {
                        provider: run.state.identity.provider,
                        provider_session_id: session,
                    })
            })
    }

    pub(crate) fn track_shell(&mut self, terminal: Uuid, root: ProcessKey) {
        self.roots.insert(root, terminal);
    }

    pub(crate) fn track_managed(&mut self, terminal: Uuid, root: ProcessKey) {
        self.managed.insert(terminal, root);
    }

    pub(crate) fn expect_hook_root(&mut self, terminal: Uuid) {
        self.starting.insert(terminal, watch::channel(None).0);
    }

    pub(crate) fn finish_hook_root(&mut self, terminal: Uuid) {
        if let Some(start) = self.starting.remove(&terminal) {
            start.send_replace(self.hook_root(terminal));
        }
    }

    pub(crate) fn hook_binding(&self, terminal: Uuid) -> Option<HookBinding> {
        if let Some(start) = self.starting.get(&terminal) {
            return Some(HookBinding::Starting(start.subscribe()));
        }
        self.hook_root(terminal).map(HookBinding::Ready)
    }

    fn hook_root(&self, terminal: Uuid) -> Option<ProcessKey> {
        self.managed.get(&terminal).copied().or_else(|| {
            self.roots
                .iter()
                .find_map(|(&root, &id)| (id == terminal).then_some(root))
        })
    }

    pub(crate) fn observer_handoff(
        &self,
        terminal: Uuid,
    ) -> (Option<Uuid>, Option<ProcessKey>, Vec<RetainedContext>) {
        (
            self.retained_observer_epoch(terminal),
            self.hook_root(terminal),
            self.retain_contexts(terminal),
        )
    }

    fn retain_contexts(&self, terminal: Uuid) -> Vec<RetainedContext> {
        self.managed
            .get(&terminal)
            .zip(self.active.get(&terminal))
            .into_iter()
            .chain(self.shell.iter().map(|(key, (id, _))| (key, id)))
            .filter_map(|(&owner, id)| {
                let run = &self.runs[id];
                if run.state.identity.terminal_id != terminal
                    || !run.live()
                    || run.state.lifecycle.value != Lifecycle::Live
                {
                    return None;
                }
                let (gap, delivery_gap) = run
                    .retained_collection
                    .unwrap_or_else(|| RetainedCoverage::from_run(&run.state))
                    .record();
                Some(RetainedContext {
                    owner,
                    producer: run.producer?,
                    provider: run.state.identity.provider,
                    session: run.state.identity.provider_session.clone()?,
                    sessions: run.sessions.clone(),
                    turn: run.state.current_open_turn().map(str::to_owned),
                    inputs: RetainedInput::collect(&run.state),
                    gap,
                    delivery_gap,
                })
            })
            .take(MAX_SHELL_RUNS)
            .collect()
    }

    pub(crate) fn recover_observers(
        &mut self,
        terminal: Uuid,
        location: Location,
        root: ProcessKey,
        epoch: Uuid,
        contexts: Vec<RetainedContext>,
    ) {
        self.retained_observers.insert(terminal, (root, epoch));
        for context in contexts {
            let Some(process) = Process::reopen(context.owner, context.provider) else {
                continue;
            };
            let id = if let Some(&id) = self.active.get(&terminal) {
                if self.managed.get(&terminal) != Some(&context.owner)
                    || self.runs[&id].state.identity.provider != context.provider
                {
                    continue;
                }
                id
            } else if self.shell.len() < MAX_SHELL_RUNS {
                let id = self.start_run(
                    terminal,
                    location,
                    context.provider,
                    ProcessLiveness::Running,
                );
                self.shell.insert(context.owner, (id, process));
                id
            } else {
                continue;
            };
            self.runs
                .get_mut(&id)
                .expect("recovered run")
                .recover_context(context);
        }
    }

    fn retained_observer_epoch(&self, terminal: Uuid) -> Option<Uuid> {
        let (root, epoch) = *self.retained_observers.get(&terminal)?;
        (self.hook_root(terminal) == Some(root)).then_some(epoch)
    }

    pub(crate) fn validate_observer_epoch(
        &self,
        terminal: Uuid,
        observer_epoch: Option<Uuid>,
        current_epoch: Uuid,
    ) -> Result<(), ProtocolFault> {
        if let Some(epoch) = observer_epoch
            && epoch != current_epoch
            && self.retained_observer_epoch(terminal) != Some(epoch)
        {
            return Err(ProtocolFault::new(
                "EpochMismatch",
                "observer launch epoch was not retained for this terminal",
            ));
        }
        Ok(())
    }

    pub(crate) fn roots(&self) -> Vec<ProcessKey> {
        self.roots.keys().copied().collect()
    }

    pub(crate) fn sample_shells(
        &mut self,
        found: Vec<Found>,
        terminals: &HashMap<Uuid, Location>,
    ) -> bool {
        let now = monotonic_ms();
        let wall = unix_ms();
        for (&id, run) in &mut self.runs {
            if run.live() && run.state.tick(now, wall) {
                self.dirty.insert(id);
            }
        }
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

    pub(crate) fn apply_hook(
        &mut self,
        hook: Hook,
        owner: HookOwner,
        location: Location,
        current_epoch: Uuid,
    ) -> Result<(), ProtocolFault> {
        let unauthorized =
            || ProtocolFault::new("Unauthorized", "hook has no verified Windows agent owner");
        let terminal = hook.association;
        let process = owner.found.process;
        let key = process.key;
        self.validate_observer_epoch(terminal, hook.observer_epoch, current_epoch)?;
        if self.hook_root(terminal) != Some(owner.found.root)
            || process.liveness() != ProcessLiveness::Running
        {
            return Err(unauthorized());
        }
        if hook.actor.is_some() || hook.parent_actor.is_some() {
            return Ok(());
        }
        // A retained process can continue its existing session without sending
        // SessionStart again. The first typed event binds only this verified owner.
        let bind_observer = self.retained_observer_epoch(terminal).is_some()
            && matches!(
                hook.event,
                HookEvent::PromptSubmitted
                    | HookEvent::QuestionStarted
                    | HookEvent::PlanApprovalStarted
            )
            && hook.session.is_some()
            && hook.turn.is_some()
            && (hook.event == HookEvent::PromptSubmitted || hook.tool.is_some());
        let id = if let Some(id) = self.active.get(&terminal).copied() {
            if self.managed.get(&terminal) != Some(&key) {
                return Err(unauthorized());
            }
            id
        } else if let Some((id, _)) = self.shell.get(&key) {
            *id
        } else if self.shell.len() < MAX_SHELL_RUNS
            && (bind_observer || matches!(hook.event, HookEvent::Startup | HookEvent::Resume))
        {
            let id = self.start_run(terminal, location, hook.provider, ProcessLiveness::Running);
            self.shell.insert(key, (id, process));
            id
        } else {
            return Err(unauthorized());
        };
        let run = &self.runs[&id];
        if !run.available
            || run.state.identity.provider != hook.provider
            || run.state.identity.terminal_id != terminal
        {
            return Err(unauthorized());
        }
        if run
            .producer
            .is_some_and(|producer| producer != owner.producer)
        {
            self.gap(
                id,
                &hook,
                RetainedCoverage::ContextGap,
                "hook producer changed",
            );
            return Err(unauthorized());
        }
        let Some(session) = hook.session.as_deref() else {
            if run.producer.is_some() {
                self.gap(
                    id,
                    &hook,
                    RetainedCoverage::DeliveryGap,
                    "hook input unavailable",
                );
            }
            return Ok(());
        };
        if matches!(
            hook.event,
            HookEvent::Startup | HookEvent::Resume | HookEvent::Clear | HookEvent::Compact
        ) {
            return self.hook_context(id, key, owner.producer, hook);
        }
        if bind_observer && run.producer.is_none() {
            let run = self.runs.get_mut(&id).expect("active run");
            Self::establish_context(run, session, owner.producer, Uuid::new_v4(), &hook);
            if hook.event != HookEvent::PromptSubmitted {
                let turn = hook.turn.clone().expect("retained input has a turn");
                let (kind, confidence) = (crate::provider::contract(hook.provider).prompt)(turn);
                run.apply(Uuid::new_v4(), kind, hook.evidence(confidence));
            }
            self.dirty.insert(id);
        }
        let run = &self.runs[&id];
        if run.producer.is_none() || run.state.identity.provider_session.as_deref() != Some(session)
        {
            return Err(unauthorized());
        }
        if let Some((kind, confidence)) = hook.observation() {
            let run = self.runs.get_mut(&id).expect("active run");
            let recovered = run.confirm_collection(&hook);
            if run.apply(hook.delivery, kind, hook.evidence(confidence)) || recovered {
                self.dirty.insert(id);
            }
        } else if matches!(
            hook.event,
            HookEvent::PromptSubmitted
                | HookEvent::QuestionResult
                | HookEvent::PlanApprovalResult
                | HookEvent::ResumeContextEnded
        ) {
            let coverage = if hook.event == HookEvent::ResumeContextEnded {
                RetainedCoverage::ContextGap
            } else {
                RetainedCoverage::DeliveryGap
            };
            self.gap(
                id,
                &hook,
                coverage,
                "hook context or correlation is incomplete",
            );
        }
        Ok(())
    }

    fn hook_context(
        &mut self,
        id: Uuid,
        key: ProcessKey,
        producer: ProcessKey,
        hook: Hook,
    ) -> Result<(), ProtocolFault> {
        let session = hook.session.as_deref().expect("context has session");
        let run = &self.runs[&id];
        if run.state.identity.provider_session.as_deref() == Some(session) {
            if self
                .runs
                .get_mut(&id)
                .expect("current context")
                .confirm_collection(&hook)
            {
                self.dirty.insert(id);
            }
            return Ok(());
        }
        if run.sessions.contains(session) {
            self.gap(
                id,
                &hook,
                RetainedCoverage::ContextGap,
                "session switch reused an uncorrelated context",
            );
            return Ok(());
        }
        if hook.event == HookEvent::Compact
            || run.sessions.len() >= MAX_CONTEXTS
            || (run.producer.is_some() && hook.event == HookEvent::Startup)
        {
            self.gap(
                id,
                &hook,
                RetainedCoverage::ContextGap,
                "hook context could not be associated",
            );
            return Ok(());
        }
        let id = if run.producer.is_some() {
            let reason = if hook.event == HookEvent::Resume {
                EndReason::ContextEnded
            } else {
                EndReason::ContextReplaced
            };
            let location = run.location;
            let mut sessions = run.sessions.clone();
            sessions.insert(session.to_owned());
            let terminal = run.state.identity.terminal_id;
            let old = self.runs.get_mut(&id).expect("tracked context");
            old.apply(
                hook.delivery,
                ObservationKind::ContextEnded(reason),
                hook.evidence(Confidence::Confirmed),
            );
            self.retire(id);
            let next = self.start_run(terminal, location, hook.provider, ProcessLiveness::Running);
            self.runs.get_mut(&next).expect("new context").sessions = sessions;
            if let Some((run, _)) = self.shell.get_mut(&key) {
                *run = next;
            } else {
                self.active.insert(terminal, next);
            }
            next
        } else {
            id
        };
        let run = self.runs.get_mut(&id).expect("current context");
        Self::establish_context(run, session, producer, hook.delivery, &hook);
        self.dirty.insert(id);
        Ok(())
    }

    fn establish_context(
        run: &mut Run,
        session: &str,
        producer: ProcessKey,
        delivery: Uuid,
        hook: &Hook,
    ) {
        run.sessions.insert(session.to_owned());
        run.producer = Some(producer);
        run.state.identity.provider_session = Some(session.to_owned());
        let evidence = hook.evidence(Confidence::Confirmed);
        run.apply(delivery, ObservationKind::ContextObserved, evidence.clone());
        run.apply(
            Uuid::new_v4(),
            ObservationKind::Collection(Collection::Healthy),
            evidence,
        );
    }

    fn gap(&mut self, id: Uuid, hook: &Hook, coverage: RetainedCoverage, reason: &str) {
        let run = self.runs.get_mut(&id).expect("tracked context");
        let coverage = if matches!(
            run.retained_collection.take(),
            Some(RetainedCoverage::ContextGap)
        ) {
            RetainedCoverage::ContextGap
        } else {
            coverage
        };
        let mut evidence = hook.evidence(Confidence::Confirmed);
        evidence.uncertainty = Some(reason.into());
        if run.apply(hook.delivery, coverage.observation(), evidence) {
            self.dirty.insert(id);
        }
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
                producer: None,
                sessions: BTreeSet::new(),
                retained_collection: None,
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
        if run.apply(
            Uuid::new_v4(),
            ObservationKind::Process(liveness),
            evidence(source, confidence),
        ) {
            self.dirty.insert(id);
        }
    }

    pub(crate) fn terminal_unavailable(&mut self, terminal: Uuid) {
        self.starting.remove(&terminal);
        self.roots.retain(|_, owner| *owner != terminal);
        self.managed.remove(&terminal);
        self.retained_observers.remove(&terminal);
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
    fn recover_context(&mut self, context: RetainedContext) {
        self.producer = Some(context.producer);
        self.sessions = context.sessions;
        self.state.identity.provider_session = Some(context.session);
        let evidence = evidence(Source::Collector, Confidence::Confirmed);
        self.apply(
            Uuid::new_v4(),
            ObservationKind::ContextObserved,
            evidence.clone(),
        );
        if let Some(turn) = context.turn {
            self.apply(
                Uuid::new_v4(),
                ObservationKind::InteractivePromptSubmitted(turn.clone()),
                evidence.clone(),
            );
            for input in context.inputs {
                input.restore(&mut self.state, &turn, &evidence);
            }
        }
        self.apply(
            Uuid::new_v4(),
            ObservationKind::Collection(Collection::Degraded),
            evidence,
        );
        self.retained_collection = Some(RetainedCoverage::from_record(
            context.gap,
            context.delivery_gap,
        ));
    }

    fn confirm_collection(&mut self, hook: &Hook) -> bool {
        let Some(collection) = self.retained_collection.take() else {
            return false;
        };
        self.apply(
            Uuid::new_v4(),
            collection.observation(),
            hook.evidence(Confidence::Confirmed),
        )
    }

    fn live(&self) -> bool {
        self.available
            && self.state.process.value == ProcessLiveness::Running
            && !matches!(self.state.lifecycle.value, Lifecycle::Ended(_))
    }

    fn apply(&mut self, delivery_id: Uuid, kind: ObservationKind, evidence: Evidence) -> bool {
        self.state.apply(
            Observation {
                run_id: self.state.identity.id,
                delivery_id,
                evidence,
                kind,
            },
            monotonic_ms(),
        )
    }

    fn info(&self) -> AgentInfo {
        let run = &self.state;
        let (process, exit_code) = projection::process(run.process.value);
        let (badge, pending_requests) = match run.badge() {
            Badge::NeedsInput(count) => (AgentBadge::NeedsInput, count as u32),
            Badge::Ended => (AgentBadge::Ended, 0),
            Badge::Working => (AgentBadge::Working, 0),
            Badge::Ready => (AgentBadge::Ready, 0),
            Badge::ResponseObserved => (AgentBadge::ResponseObserved, 0),
            Badge::Interrupted => (AgentBadge::Interrupted, 0),
            Badge::Error => (AgentBadge::Error, 0),
            Badge::Unknown => (AgentBadge::Unknown, 0),
        };
        let collection = match run.collection.value {
            Collection::NotRegistered => AgentCollection::NotRegistered,
            Collection::Healthy => AgentCollection::Healthy,
            Collection::Degraded => AgentCollection::Degraded,
            Collection::GapDetected => AgentCollection::GapDetected,
            Collection::Unsupported => AgentCollection::Unsupported,
        };
        let count = |status| {
            run.requests
                .values()
                .filter(|request| request.state.value == status)
                .count() as u32
        };
        AgentInfo {
            agent_run_id: run.identity.id.to_string(),
            terminal_id: run.identity.terminal_id.to_string(),
            location: Some(projection::selection(self.location)),
            provider: crate::provider::contract(run.identity.provider).wire_id,
            process: process as i32,
            process_observed_unix_ms: run.process.evidence.observed_unix_ms,
            exit_code,
            badge: badge as i32,
            pending_requests,
            unverified_requests: count(RequestStatus::Unverified),
            resolved_requests: count(RequestStatus::Resolved),
            cancelled_requests: count(RequestStatus::Cancelled),
            terminal_available: self.live(),
            collection: collection as i32,
            provider_session_id: run.identity.provider_session.clone(),
            response_observed_unix_ms: run
                .recent_response
                .as_ref()
                .map(|response| response.evidence.observed_unix_ms),
            end_reason: projection::end_reason(run.lifecycle.value) as i32,
            prompt_observation: projection::prompt_observation(run),
            prompt_observed_unix_ms: run
                .prompt_observation
                .as_ref()
                .map(|observation| observation.evidence.observed_unix_ms),
            adapter: Some(ProviderAdapter {
                mode: if self.producer.is_some() {
                    ProviderExecutionMode::InteractiveCli
                } else {
                    ProviderExecutionMode::ProcessOnly
                } as i32,
                schema_revision: 2,
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

pub(crate) fn evidence(source: Source, confidence: Confidence) -> Evidence {
    Evidence {
        source,
        source_version: env!("CARGO_PKG_VERSION").into(),
        observed_unix_ms: unix_ms(),
        provider_unix_ms: None,
        provider_event_id: None,
        confidence,
        uncertainty: (confidence == Confidence::Unknown)
            .then(|| "process evidence unavailable".into()),
        coverage_generation: 0,
    }
}

fn monotonic_ms() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use futures_util::FutureExt;

    use super::*;

    #[compio::test]
    async fn early_hooks_wait_for_their_registered_process() {
        let mut agents = Agents::default();
        let terminal = Uuid::new_v4();
        let other = Uuid::new_v4();
        let root = ProcessKey { pid: 7, created: 1 };
        assert!(agents.hook_binding(terminal).is_none());
        agents.expect_hook_root(terminal);
        agents.expect_hook_root(other);

        let first = agents.hook_binding(terminal).unwrap().root();
        let second = agents.hook_binding(terminal).unwrap().root();
        let cancelled = agents.hook_binding(other).unwrap().root();
        corvid_async::pin!(first, second, cancelled);
        assert!(first.as_mut().now_or_never().is_none());
        assert!(cancelled.as_mut().now_or_never().is_none());

        agents.track_managed(terminal, root);
        agents.finish_hook_root(terminal);
        agents.finish_hook_root(other);
        assert_eq!(first.await, Some(root));
        assert_eq!(second.await, Some(root));
        assert_eq!(cancelled.await, None);
        assert!(agents.hook_binding(other).is_none());
        assert_eq!(
            agents.hook_binding(terminal).unwrap().root().await,
            Some(root)
        );

        agents.terminal_unavailable(terminal);
        assert!(agents.hook_binding(terminal).is_none());
    }
}
