use std::collections::BTreeSet;

use corvid_core::agent::{
    AgentRun, Collection, Confidence, EndReason, Evidence, Lifecycle, ObservationKind,
    ProcessLiveness, Provider, RequestKey, RequestKind, Resolution, Response, ResponseOutcome,
    Source, WorkKey,
};
use corvid_protocol::ProtocolFault;
use corvid_protocol::collector::MAX_BATCH_INPUTS;
use corvid_protocol::wire::{AgentProvider, CollectHook, HookEvent};
use uuid::Uuid;

use super::{Agents, ChildKey, Launch, ManagedRun, PendingResume, evidence, monotonic_ms};
use crate::process::{HookProducer, ProcessImage};
use crate::provider::qualified_version;

const MAX_CONTEXTS_PER_LAUNCH: usize = 256;
const RESUME_WINDOW_MS: u64 = 5_000;

pub(crate) struct Hook {
    pub(crate) association: Uuid,
    delivery: Uuid,
    pub(crate) provider: Provider,
    event: HookEvent,
    session: Option<String>,
    turn: Option<String>,
    tool: Option<String>,
    actor: Option<String>,
    finished_input_ids: Vec<String>,
}

impl Hook {
    pub(crate) fn decode(
        request: &CollectHook,
        request_id: Option<&str>,
    ) -> Result<Self, ProtocolFault> {
        let invalid = || ProtocolFault::new("InvalidMessage", "invalid hook observation");
        let parse_uuid = |value: &str| Uuid::parse_str(value).ok().filter(|id| !id.is_nil());
        let association = parse_uuid(&request.launch_association).ok_or_else(invalid)?;
        let delivery = request_id.and_then(parse_uuid).ok_or_else(invalid)?;
        let event = HookEvent::try_from(request.event).map_err(|_| invalid())?;
        let provider = match AgentProvider::try_from(request.provider) {
            Ok(AgentProvider::Claude) => Provider::Claude,
            Ok(AgentProvider::Codex) => Provider::Codex,
            _ => return Err(invalid()),
        };
        let session_expected = event != HookEvent::DeliveryGap;
        if !supported_event(provider, event)
            || session_expected != request.session_id.is_some()
            || ![
                &request.session_id,
                &request.turn_id,
                &request.tool_id,
                &request.actor_id,
            ]
            .into_iter()
            .all(|id| id.as_deref().is_none_or(valid_id))
        {
            return Err(invalid());
        }
        if provider == Provider::Codex {
            let turn_expected = !matches!(
                event,
                HookEvent::Startup
                    | HookEvent::Resume
                    | HookEvent::Compact
                    | HookEvent::DeliveryGap
            );
            let tool_expected = matches!(
                event,
                HookEvent::ToolStarted
                    | HookEvent::ToolFinished
                    | HookEvent::QuestionStarted
                    | HookEvent::QuestionResult
            );
            if turn_expected != request.turn_id.is_some()
                || tool_expected != request.tool_id.is_some()
            {
                return Err(invalid());
            }
        }
        if is_child_event(event)
            && (request.actor_id.is_none()
                || request.turn_id.is_none()
                || request.tool_id.is_some())
        {
            return Err(invalid());
        }
        if !valid_input_batch(request, event) {
            return Err(invalid());
        }
        Ok(Self {
            association,
            delivery,
            provider,
            event,
            session: request.session_id.clone(),
            turn: request.turn_id.clone(),
            tool: request.tool_id.clone(),
            actor: request.actor_id.clone(),
            finished_input_ids: request.finished_input_ids.clone(),
        })
    }

    fn request_key(&self) -> Option<RequestKey> {
        Some(RequestKey {
            turn: Some(self.turn.clone()?),
            provider_request_id: self.tool.clone()?,
        })
    }

    fn work_key(&self) -> Option<WorkKey> {
        Some(WorkKey {
            turn: Some(self.turn.clone()?),
            item: self.tool.clone()?,
        })
    }

    fn evidence(&self, confidence: Confidence) -> Evidence {
        let mut evidence = hook_evidence(self.provider, confidence);
        if confidence == Confidence::Inferred {
            evidence.uncertainty = Some(
                match self.event {
                    HookEvent::ToolStarted => "other hooks can prevent the tool from running",
                    HookEvent::QuestionStarted => {
                        "tool invocation can precede or skip the question dialog"
                    }
                    _ => "other hooks can block a submitted prompt",
                }
                .to_owned(),
            );
        }
        if self.provider == Provider::Codex
            && self.event == HookEvent::ResponseObserved
            && let Some(turn) = &self.turn
        {
            let id = format!("first-stop/{turn}");
            evidence.provider_event_id = valid_id(&id).then_some(id);
        }
        evidence
    }

    fn observation(&self) -> Option<(ObservationKind, Confidence)> {
        let mut confidence = Confidence::Confirmed;
        let kind = match self.event {
            HookEvent::PromptSubmitted => {
                confidence = Confidence::Inferred;
                let turn = self.turn.clone()?;
                match self.provider {
                    Provider::Claude => ObservationKind::InteractivePromptSubmitted(turn),
                    Provider::Codex => ObservationKind::TurnStarted(turn),
                }
            }
            HookEvent::QuestionStarted => {
                if self.provider == Provider::Codex {
                    confidence = Confidence::Inferred;
                }
                ObservationKind::InputRequested {
                    key: self.request_key(),
                    kind: RequestKind::Question,
                }
            }
            HookEvent::PlanApprovalStarted => ObservationKind::InputRequested {
                key: self.request_key(),
                kind: RequestKind::PlanApproval,
            },
            HookEvent::QuestionResult | HookEvent::PlanApprovalResult => {
                ObservationKind::InputResolved {
                    key: self.request_key()?,
                    resolution: Resolution::Resolved,
                }
            }
            HookEvent::InputBatchFinished => ObservationKind::InputBatchFinished {
                turn: self.turn.clone()?,
                tools: self.finished_input_ids.clone(),
            },
            HookEvent::PermissionRequested => ObservationKind::InputRequested {
                key: None,
                kind: RequestKind::Permission,
            },
            HookEvent::ToolStarted => {
                confidence = Confidence::Inferred;
                ObservationKind::WorkStarted(self.work_key()?)
            }
            HookEvent::ToolFinished => ObservationKind::WorkFinished(self.work_key()?),
            HookEvent::TurnInterrupted => ObservationKind::Response {
                response: Response {
                    turn: Some(self.turn.clone()?),
                    outcome: ResponseOutcome::Interrupted,
                },
                final_boundary: true,
            },
            HookEvent::ResponseObserved => ObservationKind::Response {
                response: Response {
                    turn: self.turn.clone(),
                    outcome: ResponseOutcome::Observed,
                },
                final_boundary: false,
            },
            HookEvent::ContextEnded => ObservationKind::ContextEnded(EndReason::ContextEnded),
            HookEvent::ContextReplaced => ObservationKind::ContextEnded(EndReason::ContextReplaced),
            _ => return None,
        };
        Some((kind, confidence))
    }
}

fn valid_input_batch(request: &CollectHook, event: HookEvent) -> bool {
    let tools = &request.finished_input_ids;
    if event != HookEvent::InputBatchFinished {
        return tools.is_empty();
    }

    let mut seen = BTreeSet::new();
    request.turn_id.is_some()
        && request.tool_id.is_none()
        && request.actor_id.is_none()
        && (1..=MAX_BATCH_INPUTS).contains(&tools.len())
        && tools.iter().all(|tool| valid_id(tool) && seen.insert(tool))
}

fn supported_event(provider: Provider, event: HookEvent) -> bool {
    match event {
        HookEvent::Startup
        | HookEvent::Resume
        | HookEvent::Compact
        | HookEvent::PromptSubmitted
        | HookEvent::QuestionStarted
        | HookEvent::QuestionResult
        | HookEvent::PermissionRequested
        | HookEvent::ResponseObserved
        | HookEvent::DeliveryGap => true,
        HookEvent::Clear
        | HookEvent::InputBatchFinished
        | HookEvent::PlanApprovalStarted
        | HookEvent::PlanApprovalResult
        | HookEvent::ContextEnded
        | HookEvent::ContextReplaced
        | HookEvent::ResumeContextEnded
        | HookEvent::SubagentStarted
        | HookEvent::SubagentResponseObserved
        | HookEvent::SubagentLinked => provider == Provider::Claude,
        HookEvent::ToolStarted | HookEvent::ToolFinished | HookEvent::TurnInterrupted => {
            provider == Provider::Codex
        }
        HookEvent::Unspecified => false,
    }
}

fn is_child_event(event: HookEvent) -> bool {
    matches!(
        event,
        HookEvent::SubagentStarted
            | HookEvent::SubagentResponseObserved
            | HookEvent::SubagentLinked
    )
}

impl Agents {
    pub(crate) fn apply_hook(
        &mut self,
        hook: Hook,
        owner: ProcessImage,
        producer: HookProducer,
    ) -> Result<(), ProtocolFault> {
        let unauthorized = || ProtocolFault::new("Unauthorized", "hook owner is unavailable");
        let launch = self
            .launches
            .get_mut(&hook.association)
            .ok_or_else(unauthorized)?;
        if !launch.qualified || launch.owner != Some(owner) {
            return Err(unauthorized());
        }
        let current_id = launch.current;
        let current = self.runs.get_mut(&current_id).ok_or_else(unauthorized)?;
        if current.run.identity.provider != hook.provider {
            return Err(unauthorized());
        }
        if matches!(current.run.process.value, ProcessLiveness::Exited { .. }) {
            return Ok(());
        }
        if let Some(actor) = hook.actor.as_ref() {
            if is_child_event(hook.event)
                && launch.producer == Some(producer)
                && !launch.ambiguous
                && launch.resume.is_none()
                && current.run.lifecycle.value == Lifecycle::Live
                && current.run.identity.provider_session == hook.session
                && let Some(turn) = hook.turn.as_ref()
            {
                let key = ChildKey {
                    turn: turn.clone(),
                    actor: actor.clone(),
                };
                self.observe_child(
                    current_id,
                    key,
                    hook.event,
                    hook.evidence(Confidence::Confirmed),
                );
            }
            return Ok(());
        }
        self.dirty.insert(current_id);
        if launch.producer.is_some_and(|previous| previous != producer) {
            launch.ambiguous = true;
            current.terminal_available = false;
            launch.record_gap(current, "managed hook producer changed incarnation");
            return Ok(());
        }
        launch.producer = Some(producer);
        let Some(session) = hook.session.as_deref() else {
            launch.record_gap(current, "hook input was malformed or exceeded its deadline");
            return Ok(());
        };
        if matches!(
            hook.event,
            HookEvent::Startup | HookEvent::Resume | HookEvent::Clear | HookEvent::Compact
        ) {
            if let Some(next) = launch.start_context(current, &hook, session) {
                launch.current = next.run.identity.id;
                self.dirty.insert(launch.current);
                self.runs.insert(launch.current, next);
                self.retire(current_id);
            }
            return Ok(());
        }
        if current.run.identity.provider_session.as_deref() != Some(session) {
            if !launch.sessions.contains(session) {
                if hook.provider == Provider::Codex
                    && current.run.identity.provider_session.is_some()
                {
                    launch.ambiguous = true;
                    current.terminal_available = false;
                }
                launch.record_gap(
                    current,
                    "event arrived before its context could be associated",
                );
            }
            return Ok(());
        }
        if hook.event == HookEvent::ResumeContextEnded {
            launch.stage_resume(current, hook.turn);
            return Ok(());
        }
        if launch.ambiguous || launch.resume.is_some() {
            return Ok(());
        }
        if let Some((kind, confidence)) = hook.observation() {
            let evidence = hook.evidence(confidence);
            let ends_context = matches!(kind, ObservationKind::ContextEnded(_));
            if current.apply(hook.delivery, kind, evidence) && ends_context {
                self.end_children(current_id);
            }
        } else if matches!(
            hook.event,
            HookEvent::PromptSubmitted | HookEvent::QuestionResult | HookEvent::PlanApprovalResult
        ) {
            launch.record_gap(current, "correlation fields are missing");
        }
        Ok(())
    }
}

impl Launch {
    fn stage_resume(&mut self, current: &mut ManagedRun, turn: Option<String>) {
        if turn
            .as_ref()
            .is_some_and(|turn| self.resume_turns.contains(turn))
        {
            return;
        }
        current.terminal_available = false;
        let Some(turn) = turn else {
            self.abandon_resume(current, "resume context end lacks a prompt ID");
            return;
        };
        if self.resume_turns.len() == MAX_CONTEXTS_PER_LAUNCH {
            self.abandon_resume(current, "resume transition history is full");
            return;
        }
        self.resume_turns.insert(turn.clone());
        self.resume = Some(PendingResume {
            turn,
            deadline_ms: monotonic_ms().saturating_add(RESUME_WINDOW_MS),
        });
    }

    pub(super) fn expire_resume(&mut self, current: &mut ManagedRun, now: u64) -> bool {
        if self
            .resume
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline_ms)
        {
            self.abandon_resume(current, "resume context end had no matching start");
            return true;
        }
        false
    }

    fn confirm_resume(&mut self, current: &mut ManagedRun, turn: Option<&str>) -> bool {
        self.expire_resume(current, monotonic_ms());
        if self
            .resume
            .as_ref()
            .is_some_and(|pending| Some(pending.turn.as_str()) == turn)
        {
            return true;
        }
        if turn.is_none_or(|turn| !self.resume_turns.contains(turn)) {
            self.abandon_resume(current, "resume context start had no matching end");
        }
        false
    }

    fn abandon_resume(&mut self, current: &mut ManagedRun, reason: &str) {
        self.resume = None;
        self.ambiguous = true;
        current.terminal_available = false;
        self.record_gap(current, reason);
    }

    fn start_context(
        &mut self,
        current: &mut ManagedRun,
        hook: &Hook,
        session: &str,
    ) -> Option<ManagedRun> {
        if self.context_deliveries.contains(&hook.delivery) {
            return None;
        }
        self.context_deliveries.push_back(hook.delivery);
        if self.context_deliveries.len() > MAX_CONTEXTS_PER_LAUNCH {
            self.context_deliveries.pop_front();
        }

        let has_context = current.run.identity.provider_session.is_some();
        let reused = self.sessions.contains(session);
        match hook.event {
            HookEvent::Compact => {
                if !reused {
                    self.record_gap(current, "compaction has no matching context");
                }
                return None;
            }
            HookEvent::Startup if has_context => {
                if !reused {
                    if hook.provider == Provider::Codex {
                        self.ambiguous = true;
                        current.terminal_available = false;
                    }
                    self.record_gap(current, "context boundary could not be established");
                }
                return None;
            }
            HookEvent::Clear if reused => return None,
            HookEvent::Resume if has_context && hook.provider == Provider::Codex => {
                self.abandon_resume(
                    current,
                    "in-process Codex resume lacks a qualified boundary",
                );
                return None;
            }
            HookEvent::Resume if has_context => {
                if !self.confirm_resume(current, hook.turn.as_deref()) {
                    return None;
                }
            }
            _ => {}
        }
        self.resume = None;
        if !reused && self.sessions.len() == MAX_CONTEXTS_PER_LAUNCH {
            self.record_gap(current, "context identity history is full");
            return None;
        }
        self.sessions.insert(session.to_owned());
        self.ambiguous = reused;
        if !has_context {
            self.establish_context(current, hook.delivery, session);
            return None;
        }

        if !matches!(current.run.lifecycle.value, Lifecycle::Ended(_)) {
            current.apply(
                hook.delivery,
                ObservationKind::ContextEnded(EndReason::ContextReplaced),
                hook_evidence(hook.provider, Confidence::Confirmed),
            );
        }
        let mut identity = current.run.identity.clone();
        identity.id = Uuid::new_v4();
        let mut run = AgentRun::new(
            identity,
            hook_evidence(hook.provider, Confidence::Confirmed),
        );
        run.process = current.run.process.clone();
        let mut next = ManagedRun {
            run,
            location: current.location,
            process: current.process,
            qualified_version: current.qualified_version,
            published: true,
            terminal_available: true,
            children: Default::default(),
            child_coverage_gap: false,
        };
        self.establish_context(&mut next, hook.delivery, session);
        Some(next)
    }

    fn establish_context(&self, record: &mut ManagedRun, delivery: Uuid, session: &str) {
        record.run.identity.provider_session = Some(session.to_owned());
        let evidence = hook_evidence(record.run.identity.provider, Confidence::Confirmed);
        record.apply(delivery, ObservationKind::ContextObserved, evidence.clone());
        if self.ambiguous {
            mark_gap(record, "session ID reused across a context boundary");
        } else if self.gap {
            mark_gap(record, "delivery gap preceded context registration");
        } else {
            record.apply(
                Uuid::new_v4(),
                ObservationKind::Collection(Collection::Healthy),
                evidence,
            );
        }
    }

    fn record_gap(&mut self, record: &mut ManagedRun, reason: &str) {
        self.gap = true;
        mark_gap(record, reason);
    }
}

fn mark_gap(record: &mut ManagedRun, reason: &str) {
    let mut evidence = evidence(Source::Collector, Confidence::Confirmed);
    evidence.uncertainty = Some(reason.to_owned());
    record.apply(
        Uuid::new_v4(),
        ObservationKind::Collection(Collection::GapDetected),
        evidence,
    );
}

fn hook_evidence(provider: Provider, confidence: Confidence) -> Evidence {
    let mut evidence = evidence(Source::ProviderHook, confidence);
    evidence.source_version = qualified_version(provider).to_owned();
    evidence
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use corvid_core::agent::RequestStatus;
    use corvid_core::layout::Location;

    use super::*;
    use crate::process::{Executable, Incarnation};

    struct Fixture {
        agents: Agents,
        association: Uuid,
        owner: ProcessImage,
    }

    impl Fixture {
        fn new(provider: Provider) -> Self {
            let mut agents = Agents::default();
            let location = Location {
                workspace: Uuid::new_v4(),
                tab: Uuid::new_v4(),
                pane: Uuid::new_v4(),
            };
            let association = agents.reserve(location, Uuid::new_v4(), provider, true);
            let owner = ProcessImage {
                incarnation: Incarnation {
                    boot_id: Uuid::new_v4(),
                    pid: 1,
                    start_ticks: 1,
                    foreground_group: None,
                },
                executable: Executable::read(&std::env::current_exe().unwrap()).unwrap(),
            };
            let launch = agents.launches.get_mut(&association).unwrap();
            launch.owner = Some(owner);
            let record = agents.runs.get_mut(&launch.current).unwrap();
            record.published = true;
            record.terminal_available = true;
            record.observe_process(
                ProcessLiveness::Running,
                Some(owner.incarnation),
                Source::ProcessProbe,
            );
            Self {
                agents,
                association,
                owner,
            }
        }

        fn hook(&self, session: &str, event: HookEvent) -> Hook {
            Hook {
                association: self.association,
                delivery: Uuid::new_v4(),
                provider: self.current().run.identity.provider,
                event,
                session: Some(session.to_owned()),
                turn: Some("turn".to_owned()),
                tool: Some("tool".to_owned()),
                actor: None,
                finished_input_ids: Vec::new(),
            }
        }

        fn apply(&mut self, session: &str, event: HookEvent) {
            let hook = self.hook(session, event);
            self.submit(hook).unwrap();
        }

        fn child(&mut self, session: &str, actor: &str, event: HookEvent) {
            let mut hook = self.hook(session, event);
            hook.actor = Some(actor.to_owned());
            hook.tool = None;
            self.submit(hook).unwrap();
        }

        fn resume_boundary(&mut self, session: &str, event: HookEvent, turn: &str) {
            let mut hook = self.hook(session, event);
            hook.turn = Some(turn.to_owned());
            self.submit(hook).unwrap();
        }

        fn submit(&mut self, hook: Hook) -> Result<(), ProtocolFault> {
            let producer = HookProducer {
                pid: self.owner.incarnation.pid,
                start_ticks: self.owner.incarnation.start_ticks,
                executable: self.owner.executable,
            };
            self.agents.apply_hook(hook, self.owner, producer)
        }

        fn current(&self) -> &ManagedRun {
            &self.agents.runs[&self.agents.launches[&self.association].current]
        }
    }

    #[test]
    fn input_batch_requires_claude_prompt_and_distinct_tool_ids() {
        let request = CollectHook {
            launch_association: Uuid::new_v4().to_string(),
            provider: AgentProvider::Claude as i32,
            event: HookEvent::InputBatchFinished as i32,
            session_id: Some("session".into()),
            turn_id: Some("turn".into()),
            finished_input_ids: vec!["question".into(), "plan".into()],
            ..Default::default()
        };
        let delivery = Uuid::new_v4().to_string();
        assert!(Hook::decode(&request, Some(&delivery)).is_ok());
        for invalid in [
            CollectHook {
                turn_id: None,
                ..request.clone()
            },
            CollectHook {
                actor_id: Some("child".into()),
                ..request.clone()
            },
            CollectHook {
                finished_input_ids: vec!["plan".into(); 2],
                ..request.clone()
            },
            CollectHook {
                provider: AgentProvider::Codex as i32,
                ..request.clone()
            },
            CollectHook {
                event: HookEvent::ResponseObserved as i32,
                ..request
            },
        ] {
            let error = Hook::decode(&invalid, Some(&delivery)).err().unwrap();
            assert_eq!(error.code, "InvalidMessage");
        }
    }

    #[test]
    fn child_response_waits_for_parent_link_and_does_not_settle_the_parent_question() {
        let mut fixture = Fixture::new(Provider::Claude);
        fixture.apply("conversation", HookEvent::Startup);
        fixture.apply("conversation", HookEvent::QuestionStarted);
        let parent_id = fixture.current().run.identity.id;

        fixture.child("conversation", "actor", HookEvent::SubagentResponseObserved);
        fixture.child("conversation", "internal", HookEvent::SubagentStarted);
        fixture.child("conversation", "actor", HookEvent::SubagentStarted);
        fixture.child("foreign", "actor", HookEvent::SubagentLinked);
        assert_eq!(fixture.agents.runs.len(), 1);

        fixture.child("conversation", "actor", HookEvent::SubagentLinked);
        fixture.child("conversation", "actor", HookEvent::SubagentStarted);
        fixture.child("conversation", "actor", HookEvent::SubagentResponseObserved);
        assert_eq!(fixture.agents.runs.len(), 2);
        let child = fixture
            .agents
            .runs
            .values()
            .find(|record| record.run.identity.parent_run_id == Some(parent_id))
            .unwrap();
        assert_eq!(child.run.identity.actor.as_deref(), Some("actor"));
        assert!(child.run.work.is_empty());
        assert!(child.run.recent_response.is_some());
        assert_eq!(child.run.attention.len(), 1);
        assert!(
            fixture
                .agents
                .focus_terminal(child.run.identity.id)
                .is_none()
        );

        let parent = &fixture.current().run;
        assert_eq!(parent.collection.value, Collection::Healthy);
        assert!(parent.recent_response.is_none());
        assert_eq!(
            parent.requests.values().next().unwrap().state.value,
            RequestStatus::Pending
        );
    }

    #[test]
    fn children_retire_and_leave_the_inventory_with_their_parent() {
        let mut fixture = Fixture::new(Provider::Claude);
        fixture.apply("original", HookEvent::Startup);
        let parent = fixture.current().run.identity.id;
        fixture.child("original", "actor", HookEvent::SubagentLinked);
        fixture.child("original", "actor", HookEvent::SubagentStarted);
        let child = *fixture
            .agents
            .runs
            .keys()
            .find(|&&id| id != parent)
            .unwrap();
        assert!(!fixture.agents.runs[&child].run.work.is_empty());

        fixture.apply("replacement", HookEvent::Clear);
        fixture.child("original", "actor", HookEvent::SubagentResponseObserved);
        let ended = &fixture.agents.runs[&child].run;
        assert_eq!(
            ended.lifecycle.value,
            Lifecycle::Ended(EndReason::ContextEnded)
        );
        assert_eq!(ended.process.value, ProcessLiveness::Unknown);
        assert!(ended.work.is_empty());
        assert!(ended.recent_response.is_none());

        for index in 0..super::super::RETAINED_RUNS {
            fixture.apply(&format!("replacement-{index}"), HookEvent::Clear);
        }
        assert!(!fixture.agents.runs.contains_key(&parent));
        assert!(!fixture.agents.runs.contains_key(&child));
    }

    #[test]
    fn compaction_preserves_requests_and_a_matched_resume_creates_a_fresh_run() {
        let mut fixture = Fixture::new(Provider::Claude);
        fixture.apply("first", HookEvent::Startup);
        fixture.apply("first", HookEvent::QuestionStarted);
        let first = fixture.current().run.identity.id;
        let terminal = fixture.current().run.identity.terminal_id;
        let request = *fixture.current().run.requests.keys().next().unwrap();

        fixture.apply("first", HookEvent::Compact);
        assert_eq!(fixture.current().run.identity.id, first);
        assert_eq!(fixture.current().run.collection.value, Collection::Healthy);
        assert_eq!(
            fixture.current().run.requests[&request].state.value,
            RequestStatus::Pending
        );

        fixture.apply("second", HookEvent::Clear);
        let second = fixture.current().run.identity.id;
        fixture.resume_boundary("second", HookEvent::ResumeContextEnded, "switch");
        assert!(fixture.agents.focus_terminal(second).is_none());
        fixture.resume_boundary("first", HookEvent::Resume, "switch");
        let resumed = fixture.current().run.identity.id;

        // The provider can reuse session IDs and deliveries need not retain their RPC IDs.
        fixture.resume_boundary("second", HookEvent::ResumeContextEnded, "switch");
        fixture.resume_boundary("first", HookEvent::Resume, "switch");
        fixture.apply("first", HookEvent::QuestionResult);
        fixture.apply("first", HookEvent::QuestionStarted);
        assert_ne!(resumed, first);
        assert_ne!(resumed, second);
        assert_eq!(fixture.current().run.identity.id, resumed);
        assert_eq!(fixture.current().run.identity.terminal_id, terminal);
        assert_eq!(
            fixture.current().run.collection.value,
            Collection::GapDetected
        );
        assert!(fixture.current().run.requests.is_empty());
        assert!(fixture.agents.focus_terminal(resumed).is_some());
        assert_eq!(
            fixture.agents.runs[&first].run.requests[&request]
                .state
                .value,
            RequestStatus::Cancelled
        );
    }

    #[test]
    fn an_unmatched_or_expired_resume_cannot_retarget_the_run() {
        let mut fixture = Fixture::new(Provider::Claude);
        fixture.apply("first", HookEvent::Startup);
        let run = fixture.current().run.identity.id;
        fixture.resume_boundary("first", HookEvent::ResumeContextEnded, "expired");
        fixture
            .agents
            .launches
            .get_mut(&fixture.association)
            .unwrap()
            .resume
            .as_mut()
            .unwrap()
            .deadline_ms = 0;
        fixture.agents.tick();
        fixture.resume_boundary("old", HookEvent::Resume, "expired");
        fixture.resume_boundary("old", HookEvent::Resume, "different");
        fixture.apply("first", HookEvent::QuestionStarted);
        assert_eq!(fixture.current().run.identity.id, run);
        assert_eq!(
            fixture.current().run.collection.value,
            Collection::GapDetected
        );
        assert!(fixture.current().run.requests.is_empty());
        assert!(fixture.agents.focus_terminal(run).is_none());
    }

    #[test]
    fn delayed_hooks_and_child_events_cannot_mutate_the_replacement_run() {
        let mut fixture = Fixture::new(Provider::Claude);
        fixture.apply("old", HookEvent::Startup);
        fixture.apply("old", HookEvent::QuestionStarted);
        let original = fixture.current().run.identity.id;
        let terminal = fixture.current().run.identity.terminal_id;

        let clear = fixture.hook("new", HookEvent::Clear);
        let mut repeated = fixture.hook("new", HookEvent::Clear);
        repeated.delivery = clear.delivery;
        fixture.submit(clear).unwrap();
        let new_run = fixture.current().run.identity.id;
        fixture.submit(repeated).unwrap();

        fixture.apply("old", HookEvent::QuestionResult);
        fixture.apply("old", HookEvent::ContextReplaced);
        let mut child = fixture.hook("new", HookEvent::QuestionStarted);
        child.actor = Some("child".to_owned());
        fixture.submit(child).unwrap();

        assert_ne!(original, new_run);
        let previous = &fixture.agents.runs[&original];
        assert_eq!(
            previous.run.lifecycle.value,
            Lifecycle::Ended(EndReason::ContextReplaced)
        );
        assert_eq!(
            previous.run.requests.values().next().unwrap().state.value,
            RequestStatus::Cancelled
        );
        assert!(!previous.terminal_available);

        let replacement = fixture.current();
        assert_eq!(replacement.run.identity.id, new_run);
        assert_eq!(replacement.run.identity.terminal_id, terminal);
        assert_eq!(replacement.run.process.value, ProcessLiveness::Running);
        assert_eq!(replacement.run.collection.value, Collection::Healthy);
        assert!(replacement.run.requests.is_empty());
    }

    #[test]
    fn codex_keeps_distinct_provider_ownership_and_quarantines_ambiguous_startup() {
        let mut fixture = Fixture::new(Provider::Codex);
        fixture.apply("original", HookEvent::Startup);
        let run = fixture.current().run.identity.id;
        let mut foreign = fixture.hook("original", HookEvent::QuestionStarted);
        foreign.provider = Provider::Claude;
        assert_eq!(fixture.submit(foreign).unwrap_err().code, "Unauthorized");
        assert!(fixture.current().run.requests.is_empty());
        assert!(fixture.agents.focus_terminal(run).is_some());

        fixture.apply("other", HookEvent::Startup);
        fixture.apply("original", HookEvent::QuestionStarted);
        let current = fixture.current();
        assert_eq!(current.run.identity.id, run);
        assert_eq!(
            current.run.identity.provider_session.as_deref(),
            Some("original")
        );
        assert_eq!(current.run.collection.value, Collection::GapDetected);
        assert_eq!(current.run.process.value, ProcessLiveness::Running);
        assert!(current.run.requests.is_empty());
        assert!(fixture.agents.focus_terminal(run).is_none());
    }

    #[test]
    fn codex_compaction_and_fresh_resume_keep_request_history_separate() {
        let mut original = Fixture::new(Provider::Codex);
        original.apply("conversation", HookEvent::Startup);
        original.apply("conversation", HookEvent::QuestionStarted);
        original.apply("conversation", HookEvent::QuestionResult);
        let run = original.current().run.identity.id;
        let request = *original.current().run.requests.keys().next().unwrap();

        original.apply("conversation", HookEvent::Compact);
        assert_eq!(original.current().run.identity.id, run);
        assert_eq!(original.current().run.collection.value, Collection::Healthy);
        assert!(original.agents.focus_terminal(run).is_some());
        assert_eq!(
            original.current().run.requests[&request].state.value,
            RequestStatus::Resolved
        );

        original.apply("conversation", HookEvent::Resume);
        assert_eq!(original.current().run.identity.id, run);
        assert_eq!(
            original.current().run.collection.value,
            Collection::GapDetected
        );
        assert!(original.agents.focus_terminal(run).is_none());

        let mut fresh = Fixture::new(Provider::Codex);
        fresh.apply("conversation", HookEvent::Resume);
        let resumed = fresh.current();
        assert_ne!(resumed.run.identity.id, run);
        assert_eq!(
            resumed.run.identity.provider_session.as_deref(),
            Some("conversation")
        );
        assert_eq!(resumed.run.collection.value, Collection::Healthy);
        assert!(resumed.run.requests.is_empty());
        assert!(
            fresh
                .agents
                .focus_terminal(resumed.run.identity.id)
                .is_some()
        );
    }
}
