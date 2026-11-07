use super::*;

struct Fixture {
    run: AgentRun,
    now: u64,
}

impl Fixture {
    fn new() -> Self {
        let mut fixture = Self {
            run: AgentRun::new(
                RunIdentity {
                    id: Uuid::new_v4(),
                    terminal_id: Uuid::new_v4(),
                    launch_association: Uuid::new_v4(),
                    provider: Provider::Claude,
                    provider_session: Some("session".to_owned()),
                    parent_run_id: None,
                    actor: None,
                },
                evidence(Source::Launch, 0),
            ),
            now: 0,
        };
        fixture.push(ObservationKind::ContextObserved);
        fixture.push(ObservationKind::Collection(Collection::Healthy));
        fixture
    }

    fn observation(&self, kind: ObservationKind) -> Observation {
        Observation {
            run_id: self.run.identity.id,
            delivery_id: Uuid::new_v4(),
            evidence: evidence(Source::ProviderHook, self.now),
            kind,
        }
    }

    fn push(&mut self, kind: ObservationKind) -> bool {
        self.now += 1;
        let observation = self.observation(kind);
        self.run.apply(observation, self.now)
    }

    fn process(&mut self, liveness: ProcessLiveness) {
        self.now += 1;
        let mut observation = self.observation(ObservationKind::Process(liveness));
        observation.evidence.source = Source::ProcessProbe;
        assert!(self.run.apply(observation, self.now));
    }

    fn request(&mut self, turn: &str, id: &str) -> Uuid {
        let key = request_key(turn, id);
        assert!(self.push(ObservationKind::InputRequested {
            key: Some(key.clone()),
            kind: RequestKind::Question,
        }));
        self.run
            .requests
            .values()
            .find(|request| request.key.as_ref() == Some(&key))
            .unwrap()
            .id
    }

    fn resolve(&mut self, turn: &str, id: &str) -> bool {
        self.push(ObservationKind::InputResolved {
            key: request_key(turn, id),
            resolution: Resolution::Resolved,
        })
    }

    fn request_state(&self, id: Uuid) -> RequestStatus {
        self.run.requests[&id].state.value
    }

    fn turn(&mut self, id: &str) -> bool {
        self.push(ObservationKind::TurnStarted(id.to_owned()))
    }

    fn response(
        &mut self,
        turn: Option<&str>,
        outcome: ResponseOutcome,
        final_boundary: bool,
    ) -> bool {
        self.push(ObservationKind::Response {
            response: Response {
                turn: turn.map(str::to_owned),
                outcome,
            },
            final_boundary,
        })
    }
}

fn evidence(source: Source, unix_ms: u64) -> Evidence {
    Evidence {
        source,
        source_version: "fixture".to_owned(),
        observed_unix_ms: unix_ms,
        provider_unix_ms: None,
        provider_event_id: None,
        confidence: Confidence::Confirmed,
        uncertainty: None,
        coverage_generation: 0,
    }
}

fn request_key(turn: &str, id: &str) -> RequestKey {
    RequestKey {
        turn: Some(turn.to_owned()),
        provider_request_id: id.to_owned(),
    }
}

#[test]
fn acknowledgement_and_exact_resolution_leave_other_requests_pending() {
    let mut fixture = Fixture::new();
    fixture.turn("t1");
    let first = fixture.request("t1", "question");
    let second = fixture.request("t2", "question");
    let alert = fixture
        .run
        .attention
        .values()
        .find(|attention| attention.request_id == Some(first))
        .unwrap()
        .id;

    assert_eq!(fixture.run.badge(), Badge::NeedsInput(2));
    assert!(fixture.run.acknowledge(alert, fixture.now));
    assert!(!fixture.run.acknowledge(alert, fixture.now));
    assert_eq!(fixture.request_state(first), RequestStatus::Pending);
    assert_eq!(fixture.request_state(second), RequestStatus::Pending);

    assert!(fixture.resolve("t1", "question"));
    assert!(!fixture.resolve("t1", "question"));
    assert_eq!(fixture.request_state(first), RequestStatus::Resolved);
    assert_eq!(fixture.request_state(second), RequestStatus::Pending);
    assert_eq!(fixture.run.badge(), Badge::NeedsInput(1));

    let third = fixture.request("t1", "next");
    assert!(fixture.run.attention.values().any(|attention| {
        attention.request_id == Some(third) && attention.acknowledged_unix_ms.is_none()
    }));
    assert_eq!(fixture.run.badge(), Badge::NeedsInput(2));
}

#[test]
fn ambiguous_permission_is_visible_without_inventing_a_pending_request() {
    let mut fixture = Fixture::new();
    assert!(fixture.push(ObservationKind::InputRequested {
        key: None,
        kind: RequestKind::Permission
    }));
    let request = fixture.run.requests.values().next().unwrap();
    assert_eq!(request.state.value, RequestStatus::Unverified);
    assert_eq!(fixture.run.badge(), Badge::Unknown);
    assert!(!fixture.resolve("turn", "different-tool"));
    assert_eq!(
        fixture.run.requests.values().next().unwrap().state.value,
        RequestStatus::Unverified
    );
}

#[test]
fn response_and_process_exit_are_independent() {
    let mut fixture = Fixture::new();
    fixture.process(ProcessLiveness::Running);
    fixture.turn("turn");
    assert!(fixture.response(Some("turn"), ResponseOutcome::Observed, true));
    assert_eq!(fixture.run.badge(), Badge::ResponseObserved);
    assert_eq!(fixture.run.lifecycle.value, Lifecycle::Live);
    assert_eq!(fixture.run.process.value, ProcessLiveness::Running);
    assert_eq!(fixture.run.activity.evidence.source, Source::ProviderHook);

    let request = fixture.request("another-turn", "question");
    fixture.process(ProcessLiveness::Exited { code: Some(1) });
    assert_eq!(fixture.run.badge(), Badge::Ended);
    assert_eq!(
        fixture.run.lifecycle.value,
        Lifecycle::Ended(EndReason::ProcessExited)
    );
    assert_eq!(fixture.request_state(request), RequestStatus::Cancelled);
}

#[test]
fn delayed_old_stop_preserves_current_work_and_unrelated_questions() {
    let mut fixture = Fixture::new();
    fixture.turn("z-old");
    fixture.turn("a-current");
    let question = fixture.request("a-current", "question");
    assert!(fixture.response(Some("z-old"), ResponseOutcome::Failed, true));
    assert_eq!(fixture.run.activity.value, Activity::Working);
    assert_eq!(fixture.run.badge(), Badge::NeedsInput(1));
    assert_eq!(fixture.request_state(question), RequestStatus::Pending);
    assert_eq!(
        fixture.run.active_turns.keys().collect::<Vec<_>>(),
        vec!["a-current"]
    );
    assert!(!fixture.turn("z-old"));
    assert!(!fixture.response(Some("z-old"), ResponseOutcome::Failed, true));
    assert_eq!(fixture.run.attention.len(), 2);
}

#[test]
fn an_uncorrelated_response_preserves_work_and_leaves_a_response_record() {
    let mut fixture = Fixture::new();
    fixture.turn("turn");
    assert!(fixture.response(None, ResponseOutcome::Observed, true));
    assert_eq!(fixture.run.badge(), Badge::Working);
    assert!(fixture.run.recent_response.is_some());
    assert!(fixture.run.active_turns.contains_key("turn"));
}

#[test]
fn response_closes_only_the_proven_scope() {
    let mut fixture = Fixture::new();
    fixture.turn("turn");
    let work = WorkKey {
        turn: Some("turn".to_owned()),
        item: "background".to_owned(),
    };
    assert!(fixture.push(ObservationKind::WorkStarted(work.clone())));
    assert!(fixture.response(Some("turn"), ResponseOutcome::Observed, true));
    assert!(fixture.run.active_turns.is_empty());
    assert_eq!(fixture.run.badge(), Badge::Working);

    assert!(fixture.push(ObservationKind::WorkFinished(work.clone())));
    assert!(!fixture.push(ObservationKind::WorkStarted(work)));
    assert_eq!(fixture.run.badge(), Badge::Unknown);
    assert!(fixture.push(ObservationKind::PromptReady));
    assert_eq!(fixture.run.badge(), Badge::Ready);
}

#[test]
fn current_failure_keeps_its_badge_while_other_work_finishes() {
    let mut fixture = Fixture::new();
    fixture.turn("turn");
    let work = WorkKey {
        turn: Some("turn".to_owned()),
        item: "background".to_owned(),
    };
    assert!(fixture.push(ObservationKind::WorkStarted(work.clone())));
    assert!(fixture.response(Some("turn"), ResponseOutcome::Failed, true));
    assert_eq!(fixture.run.badge(), Badge::Error);
    assert!(fixture.run.work.contains_key(&work));

    assert!(fixture.push(ObservationKind::WorkFinished(work)));
    assert_eq!(fixture.run.badge(), Badge::Error);
    assert!(fixture.turn("next"));
    assert_eq!(fixture.run.badge(), Badge::Working);
}

#[test]
fn delayed_process_status_can_add_an_exit_code_without_reviving_the_run() {
    let mut fixture = Fixture::new();
    fixture.process(ProcessLiveness::Exited { code: None });

    let mut stale = fixture.observation(ObservationKind::Process(ProcessLiveness::Running));
    stale.evidence.source = Source::ProcessProbe;
    assert!(!fixture.run.apply(stale, fixture.now));

    let exit = ProcessLiveness::Exited { code: Some(2) };
    let mut waited = fixture.observation(ObservationKind::Process(exit));
    waited.evidence.source = Source::ChildExit;
    assert!(fixture.run.apply(waited, fixture.now));
    assert_eq!(fixture.run.process.value, exit);
    assert_eq!(fixture.run.badge(), Badge::Ended);
}

#[test]
fn out_of_order_result_creates_resolved_history_without_an_alert() {
    let mut fixture = Fixture::new();
    assert!(!fixture.resolve("turn", "question"));
    let request = fixture.request("turn", "question");
    assert_eq!(fixture.request_state(request), RequestStatus::Resolved);
    assert!(fixture.run.attention.is_empty());
    assert!(!fixture.push(ObservationKind::InputRequested {
        key: Some(request_key("turn", "question")),
        kind: RequestKind::Question,
    }));
}

#[test]
fn orphan_expiry_and_resync_do_not_restore_lost_evidence() {
    let mut fixture = Fixture::new();
    fixture.process(ProcessLiveness::Running);
    fixture.turn("turn");
    let pending = fixture.request("turn", "present");
    assert!(!fixture.resolve("turn", "delayed"));
    assert!(!fixture.run.tick(fixture.now + 4999, fixture.now));
    fixture.now += 5000;
    assert!(fixture.run.tick(fixture.now, fixture.now));

    assert_eq!(fixture.run.collection.value, Collection::GapDetected);
    assert_eq!(fixture.run.activity.value, Activity::Unknown);
    assert_eq!(fixture.run.process.value, ProcessLiveness::Running);
    assert_eq!(fixture.request_state(pending), RequestStatus::Unverified);
    assert_eq!(
        fixture.run.requests[&pending]
            .state
            .evidence
            .coverage_generation,
        1
    );
    let late = fixture.request("turn", "delayed");
    assert_eq!(fixture.request_state(late), RequestStatus::Unverified);

    assert!(fixture.push(ObservationKind::Collection(Collection::Healthy)));
    assert_eq!(fixture.run.badge(), Badge::Unknown);
    assert!(fixture.resolve("turn", "present"));
    assert_eq!(fixture.request_state(pending), RequestStatus::Resolved);
    assert_eq!(fixture.request_state(late), RequestStatus::Unverified);
}

#[test]
fn delivery_and_provider_event_retries_are_idempotent() {
    let mut fixture = Fixture::new();
    let kind = ObservationKind::InputRequested {
        key: None,
        kind: RequestKind::Permission,
    };
    let mut first = fixture.observation(kind.clone());
    first.evidence.provider_event_id = Some("event".to_owned());
    let delivery_id = first.delivery_id;
    assert!(fixture.run.apply(first, fixture.now));

    let mut delivery_retry = fixture.observation(kind.clone());
    delivery_retry.delivery_id = delivery_id;
    assert!(!fixture.run.apply(delivery_retry, fixture.now));

    let mut provider_retry = fixture.observation(kind.clone());
    provider_retry.evidence.provider_event_id = Some("event".to_owned());
    assert!(!fixture.run.apply(provider_retry, fixture.now));
    assert!(fixture.push(kind));
    assert_eq!(fixture.run.attention.len(), 2);
}

#[test]
fn context_replacement_never_transfers_requests_to_a_new_run() {
    let mut fixture = Fixture::new();
    fixture.process(ProcessLiveness::Running);
    let request = fixture.request("turn", "question");
    assert!(fixture.push(ObservationKind::ContextEnded(EndReason::ContextReplaced)));
    assert_eq!(fixture.request_state(request), RequestStatus::Cancelled);
    assert_eq!(fixture.run.process.value, ProcessLiveness::Running);
    assert!(!fixture.push(ObservationKind::ContextObserved));
    assert!(!fixture.turn("later"));
    fixture.process(ProcessLiveness::Exited { code: None });
    assert_eq!(
        fixture.run.lifecycle.value,
        Lifecycle::Ended(EndReason::ContextReplaced)
    );

    let delayed = fixture.observation(ObservationKind::TurnStarted("old".to_owned()));
    let mut identity = fixture.run.identity.clone();
    identity.id = Uuid::new_v4();
    let mut replacement = AgentRun::new(identity, evidence(Source::Launch, fixture.now));
    assert!(!replacement.apply(delayed, fixture.now));
    assert!(replacement.requests.is_empty());
    assert!(replacement.active_turns.is_empty());
}

#[test]
fn request_capacity_invalidates_coverage_without_fabricating_resolutions() {
    let mut fixture = Fixture::new();
    for id in 0..256 {
        fixture.request("turn", &format!("q{id}"));
    }
    assert!(fixture.push(ObservationKind::InputRequested {
        key: Some(request_key("turn", "overflow")),
        kind: RequestKind::Question,
    }));
    assert_eq!(fixture.run.requests.len(), 256);
    assert_eq!(fixture.run.attention.len(), 256);
    assert_eq!(fixture.run.collection.value, Collection::GapDetected);
    assert!(
        fixture
            .run
            .requests
            .values()
            .all(|request| request.state.value == RequestStatus::Unverified)
    );
}
