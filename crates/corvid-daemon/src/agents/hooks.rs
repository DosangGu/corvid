use corvid_core::agent::{
    AgentRun, Collection, Confidence, EndReason, Evidence, Lifecycle, ObservationKind,
    ProcessLiveness, RequestKey, RequestKind, Resolution, Response, ResponseOutcome, Source,
};
use corvid_protocol::ProtocolFault;
use corvid_protocol::collector::CLAUDE_VERSION;
use corvid_protocol::wire::{AgentProvider, CollectHook, HookEvent};
use uuid::Uuid;

use super::{Agents, Launch, ManagedRun, evidence};
use crate::process::HookOwner;

const MAX_CONTEXTS_PER_LAUNCH: usize = 256;

pub(crate) struct Hook {
    pub(crate) association: Uuid,
    delivery: Uuid,
    event: HookEvent,
    session: Option<String>,
    turn: Option<String>,
    tool: Option<String>,
    actor: Option<String>,
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
        let session_expected = event != HookEvent::DeliveryGap;
        if event == HookEvent::Unspecified
            || request.provider != AgentProvider::Claude as i32
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
        Ok(Self {
            association,
            delivery,
            event,
            session: request.session_id.clone(),
            turn: request.turn_id.clone(),
            tool: request.tool_id.clone(),
            actor: request.actor_id.clone(),
        })
    }

    fn request_key(&self) -> Option<RequestKey> {
        Some(RequestKey {
            turn: Some(self.turn.clone()?),
            provider_request_id: self.tool.clone()?,
        })
    }

    fn observation(&self) -> Option<(ObservationKind, Confidence)> {
        let mut confidence = Confidence::Confirmed;
        let kind = match self.event {
            HookEvent::PromptSubmitted => {
                confidence = Confidence::Inferred;
                ObservationKind::TurnStarted(self.turn.clone()?)
            }
            HookEvent::QuestionStarted => ObservationKind::InputRequested {
                key: self.request_key(),
                kind: RequestKind::Question,
            },
            HookEvent::QuestionResult => ObservationKind::InputResolved {
                key: self.request_key()?,
                resolution: Resolution::Resolved,
            },
            HookEvent::PermissionRequested => ObservationKind::InputRequested {
                key: None,
                kind: RequestKind::Permission,
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

impl Agents {
    pub(crate) fn apply_hook(&mut self, hook: Hook, owner: HookOwner) -> Result<(), ProtocolFault> {
        let unauthorized = || ProtocolFault::new("Unauthorized", "hook owner is unavailable");
        let launch = self
            .launches
            .get_mut(&hook.association)
            .ok_or_else(unauthorized)?;
        if launch.owner != Some(owner) {
            return Err(unauthorized());
        }
        let current_id = launch.current;
        let current = self.runs.get_mut(&current_id).ok_or_else(unauthorized)?;
        if matches!(current.run.process.value, ProcessLiveness::Exited { .. })
            || hook.actor.is_some()
        {
            return Ok(());
        }
        self.dirty.insert(current_id);
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
                launch.record_gap(
                    current,
                    "event arrived before its context could be associated",
                );
            }
            return Ok(());
        }
        if launch.ambiguous {
            return Ok(());
        }
        if let Some((kind, confidence)) = hook.observation() {
            current.apply(hook.delivery, kind, hook_evidence(confidence));
        } else if matches!(
            hook.event,
            HookEvent::PromptSubmitted | HookEvent::QuestionResult
        ) {
            launch.record_gap(current, "correlation fields are missing");
        }
        Ok(())
    }
}

impl Launch {
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

        let existing = current.run.identity.provider_session.as_deref();
        let reused = existing == Some(session);
        let replacement = matches!(hook.event, HookEvent::Clear | HookEvent::Resume);
        if reused && (!replacement || self.ambiguous) {
            return None;
        }
        if !reused && self.sessions.contains(session) {
            return None;
        }
        if hook.event == HookEvent::Compact
            || (existing.is_some() && hook.event == HookEvent::Startup)
        {
            self.record_gap(current, "context boundary could not be established");
            return None;
        }
        if self.sessions.len() == MAX_CONTEXTS_PER_LAUNCH {
            self.record_gap(current, "context identity history is full");
            return None;
        }
        self.sessions.insert(session.to_owned());
        self.ambiguous = reused;
        if existing.is_none() {
            self.establish_context(current, hook.delivery, session);
            return None;
        }

        if !matches!(current.run.lifecycle.value, Lifecycle::Ended(_)) {
            current.apply(
                hook.delivery,
                ObservationKind::ContextEnded(EndReason::ContextReplaced),
                hook_evidence(Confidence::Confirmed),
            );
        }
        let mut identity = current.run.identity.clone();
        identity.id = Uuid::new_v4();
        let mut run = AgentRun::new(identity, hook_evidence(Confidence::Confirmed));
        run.process = current.run.process.clone();
        let mut next = ManagedRun {
            run,
            location: current.location,
            process: current.process,
            qualified_version: current.qualified_version,
            published: true,
            terminal_available: true,
        };
        self.establish_context(&mut next, hook.delivery, session);
        Some(next)
    }

    fn establish_context(&self, record: &mut ManagedRun, delivery: Uuid, session: &str) {
        record.run.identity.provider_session = Some(session.to_owned());
        let evidence = hook_evidence(Confidence::Confirmed);
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

fn hook_evidence(confidence: Confidence) -> Evidence {
    let mut evidence = evidence(Source::ProviderHook, confidence);
    evidence.source_version = CLAUDE_VERSION.to_owned();
    if confidence == Confidence::Inferred {
        evidence.uncertainty = Some("other hooks can block a submitted prompt".to_owned());
    }
    evidence
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use corvid_core::agent::{Provider, RequestStatus};
    use corvid_core::layout::Location;

    use super::*;
    use crate::process::{Executable, Incarnation};

    struct Fixture {
        agents: Agents,
        association: Uuid,
        owner: HookOwner,
    }

    impl Fixture {
        fn new() -> Self {
            let mut agents = Agents::default();
            let location = Location {
                workspace: Uuid::new_v4(),
                tab: Uuid::new_v4(),
                pane: Uuid::new_v4(),
            };
            let association = agents.reserve(location, Uuid::new_v4(), Provider::Claude, true);
            let owner = HookOwner {
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
                event,
                session: Some(session.to_owned()),
                turn: Some("turn".to_owned()),
                tool: Some("tool".to_owned()),
                actor: None,
            }
        }

        fn apply(&mut self, session: &str, event: HookEvent) {
            let hook = self.hook(session, event);
            self.agents.apply_hook(hook, self.owner).unwrap();
        }

        fn current(&self) -> &ManagedRun {
            &self.agents.runs[&self.agents.launches[&self.association].current]
        }
    }

    #[test]
    fn delayed_hooks_and_child_events_cannot_mutate_the_replacement_run() {
        let mut fixture = Fixture::new();
        fixture.apply("old", HookEvent::Startup);
        fixture.apply("old", HookEvent::QuestionStarted);
        let original = fixture.current().run.identity.id;
        let terminal = fixture.current().run.identity.terminal_id;

        let clear = fixture.hook("new", HookEvent::Clear);
        let mut repeated = fixture.hook("new", HookEvent::Clear);
        repeated.delivery = clear.delivery;
        fixture.agents.apply_hook(clear, fixture.owner).unwrap();
        let new_run = fixture.current().run.identity.id;
        fixture.agents.apply_hook(repeated, fixture.owner).unwrap();

        fixture.apply("old", HookEvent::QuestionResult);
        fixture.apply("old", HookEvent::ContextReplaced);
        let mut child = fixture.hook("new", HookEvent::QuestionStarted);
        child.actor = Some("child".to_owned());
        fixture.agents.apply_hook(child, fixture.owner).unwrap();

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
}
