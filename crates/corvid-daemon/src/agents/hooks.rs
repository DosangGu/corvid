use corvid_core::agent::{
    AgentRun, Collection, Confidence, EndReason, Lifecycle, ObservationKind, ProcessLiveness,
    Provider, ResponseOutcome, Source,
};
use corvid_core::layout::Location;
use corvid_protocol::ProtocolFault;
use corvid_protocol::wire::HookEvent;
#[cfg(test)]
use corvid_protocol::wire::{AgentProvider, CollectHook};
use uuid::Uuid;

use super::children::{ChildEvent, ChildOwner};
use super::{Agents, ChildKey, Launch, ManagedRun, PendingResume, evidence, monotonic_ms};
use crate::hook_data::{Hook, hook_evidence};
use crate::process::{HookProducer, ProcessImage};

const MAX_CONTEXTS_PER_LAUNCH: usize = 256;
const RESUME_WINDOW_MS: u64 = 5_000;

impl Hook {
    fn child_event(&self) -> Option<ChildEvent> {
        Some(match self.event {
            HookEvent::SubagentStarted => ChildEvent::Started,
            HookEvent::SubagentResponseObserved => ChildEvent::Responded,
            HookEvent::SubagentLinked => ChildEvent::Linked(ChildOwner::Root),
            HookEvent::NestedSubagentLinked => {
                ChildEvent::Linked(ChildOwner::Actor(self.parent_actor.clone()?))
            }
            _ => return None,
        })
    }
}

impl Agents {
    pub(crate) fn apply_hook(
        &mut self,
        hook: Hook,
        owner: ProcessImage,
        producer: HookProducer,
    ) -> Result<Option<(Location, Provider)>, ProtocolFault> {
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
            return Ok(None);
        }
        if !current.published {
            if hook.actor.is_some() || !matches!(hook.event, HookEvent::Startup | HookEvent::Resume)
            {
                return Err(unauthorized());
            }
            // Shell launches become visible only after the real provider proves ownership.
            current.observe_process(
                ProcessLiveness::Running,
                Some(owner.incarnation),
                Source::ProcessProbe,
            );
            current.published = true;
            current.terminal_available = true;
        }
        if let Some(actor) = hook.actor.as_ref() {
            if launch.producer == Some(producer)
                && !launch.ambiguous
                && launch.resume.is_none()
                && current.run.lifecycle.value == Lifecycle::Live
                && current.run.identity.provider_session == hook.session
                && let Some(turn) = hook.turn.as_ref()
                && let Some(event) = hook.child_event()
            {
                let key = ChildKey {
                    turn: turn.clone(),
                    actor: actor.clone(),
                };
                self.observe_child(current_id, key, event, hook.evidence(Confidence::Confirmed));
            }
            return Ok(None);
        }
        self.dirty.insert(current_id);
        if launch.producer.is_some_and(|previous| previous != producer) {
            launch.ambiguous = true;
            current.terminal_available = false;
            launch.record_gap(current, "managed hook producer changed incarnation");
            return Ok(None);
        }
        launch.producer = Some(producer);
        let Some(session) = hook.session.as_deref() else {
            launch.record_gap(current, "hook input was malformed or exceeded its deadline");
            return Ok(None);
        };
        if matches!(
            hook.event,
            HookEvent::Startup | HookEvent::Resume | HookEvent::Clear | HookEvent::Compact
        ) {
            if let Some(next) = launch.start_context(current, &hook, session) {
                launch.current = next.run.identity.id;
                launch.notified_turn = None;
                self.dirty.insert(launch.current);
                self.runs.insert(launch.current, next);
                self.retire(current_id);
            }
            return Ok(None);
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
            return Ok(None);
        }
        if hook.event == HookEvent::ResumeContextEnded {
            launch.stage_resume(current, hook.turn);
            return Ok(None);
        }
        if launch.ambiguous || launch.resume.is_some() {
            return Ok(None);
        }
        if let Some((kind, confidence)) = hook.observation() {
            let evidence = hook.evidence(confidence);
            let ends_context = matches!(kind, ObservationKind::ContextEnded(_));
            let notify_turn = response_notification_turn(&current.run, &kind, confidence)
                .filter(|turn| launch.notified_turn.as_ref() != Some(turn));
            if current.apply(hook.delivery, kind, evidence) {
                if ends_context {
                    self.end_children(current_id);
                } else if let Some(turn) = notify_turn
                    && current.run.collection.value == Collection::Healthy
                {
                    launch.notified_turn = Some(turn);
                    return Ok(Some((current.location, hook.provider)));
                }
            }
        } else if matches!(
            hook.event,
            HookEvent::PromptSubmitted | HookEvent::QuestionResult | HookEvent::PlanApprovalResult
        ) {
            launch.record_gap(current, "correlation fields are missing");
        }
        Ok(None)
    }
}

fn response_notification_turn(
    run: &AgentRun,
    kind: &ObservationKind,
    confidence: Confidence,
) -> Option<String> {
    let ObservationKind::Response { response, .. } = kind else {
        return None;
    };
    let turn = response.turn.as_deref()?;
    (confidence == Confidence::Confirmed
        && response.outcome == ResponseOutcome::Observed
        && run.lifecycle.value == Lifecycle::Live
        && run.collection.value == Collection::Healthy
        && run.is_current_open_turn(turn))
    .then(|| turn.to_owned())
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
        self.resume_key = None;
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
        self.resume_key = None;
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

    fn establish_context(&mut self, record: &mut ManagedRun, delivery: Uuid, session: &str) {
        record.run.identity.provider_session = Some(session.to_owned());
        self.resume_key = Some(corvid_core::agent::AgentSessionKey {
            provider: record.run.identity.provider,
            provider_session_id: session.to_owned(),
        });
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
        self.resume_key = None;
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

#[cfg(test)]
mod tests {
    use corvid_core::agent::RequestStatus;
    use corvid_core::layout::Location;

    use super::*;
    use crate::process::{Executable, Incarnation};

    #[test]
    fn saved_resume_identity_tracks_correlated_switches_but_not_ambiguous_codex_switches() {
        let mut fixture = Fixture::new(Provider::Claude);
        fixture.apply("first", HookEvent::Startup);
        let terminal = fixture.current().run.identity.terminal_id;
        assert_eq!(
            fixture
                .agents
                .confirmed_session(terminal)
                .unwrap()
                .provider_session_id,
            "first"
        );
        fixture.resume_boundary("first", HookEvent::ResumeContextEnded, "switch");
        assert!(fixture.agents.confirmed_session(terminal).is_none());
        fixture.resume_boundary("second", HookEvent::Resume, "switch");
        assert_eq!(
            fixture
                .agents
                .confirmed_session(terminal)
                .unwrap()
                .provider_session_id,
            "second"
        );
        fixture.resume_boundary("second", HookEvent::ResumeContextEnded, "return");
        fixture.resume_boundary("first", HookEvent::Resume, "return");
        assert_eq!(
            fixture
                .agents
                .confirmed_session(terminal)
                .unwrap()
                .provider_session_id,
            "first"
        );

        let mut codex = Fixture::new(Provider::Codex);
        codex.apply("first", HookEvent::Startup);
        let terminal = codex.current().run.identity.terminal_id;
        codex.apply("second", HookEvent::Resume);
        assert!(codex.agents.confirmed_session(terminal).is_none());
    }

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
                parent_actor: None,
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

        fn nested_link(&mut self, session: &str, actor: &str, parent: &str) {
            let mut hook = self.hook(session, HookEvent::NestedSubagentLinked);
            hook.actor = Some(actor.to_owned());
            hook.parent_actor = Some(parent.to_owned());
            hook.tool = None;
            self.submit(hook).unwrap();
        }

        fn resume_boundary(&mut self, session: &str, event: HookEvent, turn: &str) {
            let mut hook = self.hook(session, event);
            hook.turn = Some(turn.to_owned());
            self.submit(hook).unwrap();
        }

        fn submit(&mut self, hook: Hook) -> Result<(), ProtocolFault> {
            self.submit_notice(hook).map(drop)
        }

        fn submit_notice(
            &mut self,
            hook: Hook,
        ) -> Result<Option<(Location, Provider)>, ProtocolFault> {
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
    fn notifies_only_the_first_verified_response_for_the_current_prompt() {
        for provider in [Provider::Claude, Provider::Codex] {
            let mut fixture = Fixture::new(provider);
            fixture.apply("session", HookEvent::Startup);

            let without_prompt = fixture.hook("session", HookEvent::ResponseObserved);
            assert_eq!(fixture.submit_notice(without_prompt).unwrap(), None);

            let mut prompt = fixture.hook("session", HookEvent::PromptSubmitted);
            prompt.turn = Some("current".to_owned());
            fixture.submit(prompt).unwrap();
            let late = fixture.hook("session", HookEvent::ResponseObserved);
            assert_eq!(fixture.submit_notice(late).unwrap(), None);

            let mut first = fixture.hook("session", HookEvent::ResponseObserved);
            first.turn = Some("current".to_owned());
            assert_eq!(
                fixture.submit_notice(first).unwrap(),
                Some((fixture.current().location, provider))
            );
            let mut unscoped = fixture.hook("session", HookEvent::ResponseObserved);
            unscoped.turn = None;
            assert_eq!(fixture.submit_notice(unscoped).unwrap(), None);
            let mut repeated = fixture.hook("session", HookEvent::ResponseObserved);
            repeated.turn = Some("current".to_owned());
            assert_eq!(fixture.submit_notice(repeated).unwrap(), None);

            let mut next = fixture.hook("session", HookEvent::PromptSubmitted);
            next.turn = Some("next".to_owned());
            fixture.submit(next).unwrap();
            let mut response = fixture.hook("session", HookEvent::ResponseObserved);
            response.turn = Some("next".to_owned());
            let expected = if provider == Provider::Codex {
                let mut interrupted = fixture.hook("session", HookEvent::TurnInterrupted);
                interrupted.turn = Some("next".to_owned());
                fixture.submit(interrupted).unwrap();
                None
            } else {
                Some((fixture.current().location, provider))
            };
            assert_eq!(fixture.submit_notice(response).unwrap(), expected);
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
    fn nested_link_requires_the_distinct_parent_actor() {
        let request = CollectHook {
            launch_association: Uuid::new_v4().to_string(),
            provider: AgentProvider::Claude as i32,
            event: HookEvent::NestedSubagentLinked as i32,
            session_id: Some("session".into()),
            turn_id: Some("turn".into()),
            actor_id: Some("child".into()),
            parent_actor_id: Some("parent".into()),
            ..Default::default()
        };
        let delivery = Uuid::new_v4().to_string();
        assert!(Hook::decode(&request, Some(&delivery)).is_ok());
        for invalid in [
            CollectHook {
                parent_actor_id: None,
                ..request.clone()
            },
            CollectHook {
                parent_actor_id: request.actor_id.clone(),
                ..request.clone()
            },
            CollectHook {
                event: HookEvent::SubagentLinked as i32,
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
    fn nested_children_wait_for_an_anchored_parent_and_cannot_be_reparented() {
        let mut fixture = Fixture::new(Provider::Claude);
        fixture.apply("conversation", HookEvent::Startup);
        fixture.apply("conversation", HookEvent::QuestionStarted);
        let root = fixture.current().run.identity.id;

        fixture.child("conversation", "outer", HookEvent::SubagentStarted);
        fixture.child("conversation", "inner", HookEvent::SubagentStarted);
        fixture.child("conversation", "inner", HookEvent::SubagentResponseObserved);
        fixture.nested_link("conversation", "inner", "outer");
        for (actor, parent) in [("cycle-a", "cycle-b"), ("cycle-b", "cycle-a")] {
            fixture.child("conversation", actor, HookEvent::SubagentStarted);
            fixture.nested_link("conversation", actor, parent);
        }
        assert_eq!(fixture.agents.runs.len(), 1);

        fixture.child("conversation", "outer", HookEvent::SubagentLinked);
        assert_eq!(fixture.agents.runs.len(), 3);
        let outer = fixture
            .agents
            .runs
            .values()
            .find(|record| record.run.identity.actor.as_deref() == Some("outer"))
            .unwrap();
        assert_eq!(outer.run.identity.parent_run_id, Some(root));
        assert!(outer.run.recent_response.is_none());
        let outer_id = outer.run.identity.id;
        let inner = fixture
            .agents
            .runs
            .values()
            .find(|record| record.run.identity.actor.as_deref() == Some("inner"))
            .unwrap();
        assert_eq!(inner.run.identity.parent_run_id, Some(outer_id));
        assert!(inner.run.recent_response.is_some());
        let inner_id = inner.run.identity.id;

        fixture.child("conversation", "inner", HookEvent::SubagentLinked);
        assert_eq!(fixture.agents.runs.len(), 3);
        assert!(fixture.current().child_coverage_gap);
        let inner = &fixture.agents.runs[&inner_id].run;
        assert_eq!(inner.identity.parent_run_id, Some(outer_id));
        assert_eq!(inner.collection.value, Collection::GapDetected);
        assert_eq!(
            fixture
                .current()
                .run
                .requests
                .values()
                .next()
                .unwrap()
                .state
                .value,
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
