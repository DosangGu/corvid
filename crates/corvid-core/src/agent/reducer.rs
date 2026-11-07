use std::collections::{BTreeMap, BTreeSet, VecDeque};

use uuid::Uuid;

use super::{
    Activity, AgentRun, Attention, AttentionCause, Collection, Confidence, EndReason, Evidence,
    InputRequest, Lifecycle, Observation, ObservationKind, Observed, ProcessLiveness, RequestKey,
    RequestKind, RequestStatus, Resolution, Response, ResponseOutcome, Source, WorkKey,
};

const MAX_SCOPES: usize = 256;
const MAX_REQUESTS: usize = 256;
const MAX_ATTENTION: usize = 256;
const MAX_RECENT_KEYS: usize = 1024;
const MAX_ORPHANS: usize = 256;
const ORPHAN_WINDOW_MS: u64 = 5_000;
const MAX_ID_BYTES: usize = 256;
const MAX_EVIDENCE_TEXT_BYTES: usize = 1024;

#[derive(Default)]
pub(super) struct Correlation {
    current_turn: Option<String>,
    pub(super) response_for_prompt: bool,
    generation: u64,
    deliveries: Recent<Uuid>,
    provider_events: Recent<(Source, String)>,
    tentative_turns: Recent<String>,
    completed_turns: Recent<String>,
    completed_work: Recent<WorkKey>,
    finished_requests: Recent<RequestKey>,
    orphan_results: BTreeMap<RequestKey, OrphanResult>,
    expired_results: Recent<RequestKey>,
}

struct OrphanResult {
    deadline_ms: u64,
    state: Observed<RequestStatus>,
}

struct Recent<T> {
    order: VecDeque<T>,
    keys: BTreeSet<T>,
}

impl<T> Default for Recent<T> {
    fn default() -> Self {
        Self {
            order: VecDeque::new(),
            keys: BTreeSet::new(),
        }
    }
}

impl<T: Clone + Ord> Recent<T> {
    fn insert(&mut self, key: T) -> bool {
        if !self.keys.insert(key.clone()) {
            return false;
        }
        self.order.push_back(key);
        if self.order.len() > MAX_RECENT_KEYS
            && let Some(oldest) = self.order.pop_front()
        {
            self.keys.remove(&oldest);
        }
        true
    }

    fn contains(&self, key: &T) -> bool {
        self.keys.contains(key)
    }
}

impl AgentRun {
    /// Apply a normalized observation to this run. The caller authenticates its owner
    /// and supplies daemon receipt times. The return value signals a visible change.
    pub fn apply(&mut self, observation: Observation, monotonic_ms: u64) -> bool {
        if observation.run_id != self.identity.id || !observation.is_bounded() {
            return false;
        }

        let mut evidence = observation.evidence;
        let expired = self.tick(monotonic_ms, evidence.observed_unix_ms);
        if !self.correlation.deliveries.insert(observation.delivery_id) {
            return expired;
        }
        if let Some(event_id) = &evidence.provider_event_id
            && !self
                .correlation
                .provider_events
                .insert((evidence.source, event_id.clone()))
        {
            return expired;
        }

        evidence.coverage_generation = self.correlation.generation;
        let kind = observation.kind;
        let changed = match kind {
            ObservationKind::Process(liveness) => self.observe_process(liveness, &evidence),
            _ if matches!(self.lifecycle.value, Lifecycle::Ended(_)) => false,
            _ => self.apply_live(kind, &evidence, monotonic_ms),
        };
        expired || changed
    }

    /// Expire unmatched results using the daemon's monotonic clock.
    pub fn tick(&mut self, monotonic_ms: u64, unix_ms: u64) -> bool {
        let mut expired = false;
        let correlation = &mut self.correlation;
        correlation.orphan_results.retain(|key, result| {
            if monotonic_ms < result.deadline_ms {
                return true;
            }
            correlation.expired_results.insert(key.clone());
            expired = true;
            false
        });
        if expired {
            self.record_gap(unix_ms, "unmatched input result expired");
        }
        expired
    }

    fn apply_live(
        &mut self,
        kind: ObservationKind,
        evidence: &Evidence,
        monotonic_ms: u64,
    ) -> bool {
        match kind {
            ObservationKind::ContextObserved if evidence.confidence == Confidence::Confirmed => {
                replace(&mut self.lifecycle, Lifecycle::Live, evidence)
            }
            ObservationKind::TurnStarted(turn) => self.start_turn(turn, evidence),
            ObservationKind::WorkStarted(key) => self.start_work(key, evidence),
            ObservationKind::WorkFinished(key) => self.finish_work(key, evidence),
            ObservationKind::InputRequested { key, kind } => {
                self.request_input(key, kind, evidence)
            }
            ObservationKind::InputResolved { key, resolution } => {
                self.resolve_input(key, resolution, evidence, monotonic_ms)
            }
            ObservationKind::Response {
                response,
                final_boundary,
            } => self.observe_response(response, final_boundary, evidence),
            ObservationKind::PromptReady if self.can_assert_wait(evidence) => {
                let had_response = std::mem::take(&mut self.correlation.response_for_prompt);
                replace(&mut self.activity, Activity::AwaitingPrompt, evidence) || had_response
            }
            ObservationKind::ContextEnded(reason)
                if evidence.confidence == Confidence::Confirmed =>
            {
                self.end(reason, evidence);
                true
            }
            ObservationKind::Collection(health) => self.observe_collection(health, evidence),
            _ => false,
        }
    }

    fn start_turn(&mut self, turn: String, evidence: &Evidence) -> bool {
        if self.correlation.completed_turns.contains(&turn) || self.active_turns.contains_key(&turn)
        {
            return false;
        }
        let confirmed = evidence.confidence == Confidence::Confirmed;
        if confirmed && self.active_turns.len() == MAX_SCOPES {
            self.record_gap(evidence.observed_unix_ms, "active turn limit reached");
            return true;
        }
        if !confirmed && !self.correlation.tentative_turns.insert(turn.clone()) {
            return false;
        }
        self.correlation.current_turn = Some(turn.clone());
        self.correlation.response_for_prompt = false;
        self.activity = Observed::new(Activity::Unknown, evidence);
        if confirmed {
            self.active_turns.insert(turn, evidence.clone());
        }
        self.refresh_work_activity(evidence);
        true
    }

    fn start_work(&mut self, key: WorkKey, evidence: &Evidence) -> bool {
        if self.correlation.completed_work.contains(&key) || self.work.contains_key(&key) {
            return false;
        }
        if self.work.len() == MAX_SCOPES {
            self.record_gap(evidence.observed_unix_ms, "active work limit reached");
            return true;
        }
        self.work.insert(key, evidence.clone());
        self.refresh_work_activity(evidence);
        true
    }

    fn finish_work(&mut self, key: WorkKey, evidence: &Evidence) -> bool {
        if evidence.confidence != Confidence::Confirmed {
            return false;
        }
        let closed = self.work.remove(&key).is_some();
        self.correlation.completed_work.insert(key);
        if closed {
            self.refresh_work_activity(evidence);
        }
        closed
    }

    fn refresh_work_activity(&mut self, evidence: &Evidence) {
        if matches!(self.activity.value, Activity::Interrupted | Activity::Error) {
            return;
        }
        let confirmed = self
            .active_turns
            .values()
            .chain(self.work.values())
            .find(|scope| {
                scope.confidence == Confidence::Confirmed
                    && scope.coverage_generation == self.correlation.generation
            });
        if let Some(confirmed) = confirmed {
            self.correlation.response_for_prompt = false;
            self.activity = Observed::new(Activity::Working, confirmed);
        } else if self.activity.value == Activity::Working
            || !self.work.is_empty()
            || !self.active_turns.is_empty()
        {
            let evidence = evidence.uncertain("no currently confirmed active work");
            self.activity = Observed::new(Activity::Unknown, &evidence);
        }
    }

    fn request_input(
        &mut self,
        key: Option<RequestKey>,
        kind: RequestKind,
        evidence: &Evidence,
    ) -> bool {
        if let Some(key) = &key
            && (self.correlation.finished_requests.contains(key) || self.request_id(key).is_some())
        {
            return false;
        }
        if self.requests.len() == MAX_REQUESTS {
            self.prune_closed_request();
            if self.requests.len() == MAX_REQUESTS {
                self.record_gap(evidence.observed_unix_ms, "input request limit reached");
                return true;
            }
        }

        let state = self.initial_request_state(key.as_ref(), evidence);
        let id = Uuid::new_v4();
        if is_open(state.value) {
            self.add_attention(AttentionCause::Input, Some(id), evidence.observed_unix_ms);
        }
        self.requests.insert(
            id,
            InputRequest {
                id,
                key,
                kind,
                state,
            },
        );
        true
    }

    fn initial_request_state(
        &mut self,
        key: Option<&RequestKey>,
        evidence: &Evidence,
    ) -> Observed<RequestStatus> {
        if let Some(key) = key {
            if let Some(result) = self.correlation.orphan_results.remove(key) {
                self.correlation.finished_requests.insert(key.clone());
                return result.state;
            }
            if evidence.confidence == Confidence::Confirmed
                && !self.correlation.expired_results.contains(key)
            {
                return Observed::new(RequestStatus::Pending, evidence);
            }
        }

        let uncertain = evidence.uncertain("input request correlation is incomplete");
        Observed::new(RequestStatus::Unverified, &uncertain)
    }

    fn resolve_input(
        &mut self,
        key: RequestKey,
        resolution: Resolution,
        evidence: &Evidence,
        monotonic_ms: u64,
    ) -> bool {
        if evidence.confidence != Confidence::Confirmed
            || self.correlation.finished_requests.contains(&key)
            || self.correlation.orphan_results.contains_key(&key)
            || self.correlation.expired_results.contains(&key)
        {
            return false;
        }
        let state = Observed::new(resolution.into(), evidence);
        if let Some(id) = self.request_id(&key) {
            let request = self.requests.get_mut(&id).expect("matched request exists");
            if !is_open(request.state.value) {
                return false;
            }
            request.state = state;
            self.correlation.finished_requests.insert(key);
            return true;
        }
        if self.correlation.orphan_results.len() == MAX_ORPHANS {
            self.correlation.expired_results.insert(key);
            self.record_gap(
                evidence.observed_unix_ms,
                "unmatched input result limit reached",
            );
            return true;
        }
        let deadline_ms = monotonic_ms.saturating_add(ORPHAN_WINDOW_MS);
        self.correlation
            .orphan_results
            .insert(key, OrphanResult { deadline_ms, state });
        false
    }

    fn request_id(&self, key: &RequestKey) -> Option<Uuid> {
        self.requests
            .values()
            .find(|request| request.key.as_ref() == Some(key))
            .map(|request| request.id)
    }

    fn prune_closed_request(&mut self) {
        let oldest = self
            .requests
            .values()
            .filter(|request| !is_open(request.state.value))
            .min_by_key(|request| request.state.evidence.observed_unix_ms)
            .map(|request| request.id);
        if let Some(id) = oldest {
            self.requests.remove(&id);
        }
    }

    fn observe_response(
        &mut self,
        response: Response,
        final_boundary: bool,
        evidence: &Evidence,
    ) -> bool {
        let mut current_boundary = false;
        if final_boundary
            && evidence.confidence == Confidence::Confirmed
            && let Some(turn) = &response.turn
        {
            if !self.correlation.completed_turns.insert(turn.clone()) {
                return false;
            }
            let observed_start = self.active_turns.remove(turn).is_some();
            current_boundary =
                observed_start && self.correlation.current_turn.as_ref() == Some(turn);
            self.refresh_work_activity(evidence);
        }

        let cause = match response.outcome {
            ResponseOutcome::Observed => AttentionCause::Response,
            ResponseOutcome::Interrupted | ResponseOutcome::Failed => AttentionCause::Failure,
        };
        self.add_attention(cause, None, evidence.observed_unix_ms);

        if current_boundary && self.collection.value == Collection::Healthy {
            let activity = match response.outcome {
                ResponseOutcome::Observed if self.can_assert_wait(evidence) => {
                    Some(Activity::AwaitingPrompt)
                }
                ResponseOutcome::Observed => None,
                ResponseOutcome::Interrupted => Some(Activity::Interrupted),
                ResponseOutcome::Failed => Some(Activity::Error),
            };
            if let Some(activity) = activity {
                self.activity = Observed::new(activity, evidence);
                self.correlation.response_for_prompt = activity == Activity::AwaitingPrompt;
            }
        }
        self.recent_response = Some(Observed::new(response, evidence));
        true
    }

    fn can_assert_wait(&self, evidence: &Evidence) -> bool {
        evidence.confidence == Confidence::Confirmed
            && self.collection.value == Collection::Healthy
            && self.active_turns.is_empty()
            && self.work.is_empty()
    }

    fn observe_process(&mut self, value: ProcessLiveness, evidence: &Evidence) -> bool {
        if !matches!(evidence.source, Source::ProcessProbe | Source::ChildExit)
            || (value != ProcessLiveness::Unknown && evidence.confidence != Confidence::Confirmed)
        {
            return false;
        }
        if let ProcessLiveness::Exited { code } = self.process.value {
            return code.is_none()
                && matches!(value, ProcessLiveness::Exited { code: Some(_) })
                && replace(&mut self.process, value, evidence);
        }
        let changed = replace(&mut self.process, value, evidence);
        if matches!(value, ProcessLiveness::Exited { .. })
            && !matches!(self.lifecycle.value, Lifecycle::Ended(_))
        {
            self.end(EndReason::ProcessExited, evidence);
            return true;
        }
        changed
    }

    fn end(&mut self, reason: EndReason, evidence: &Evidence) {
        self.lifecycle = Observed::new(Lifecycle::Ended(reason), evidence);
        self.activity = Observed::new(Activity::Unknown, evidence);
        self.active_turns.clear();
        self.work.clear();
        self.correlation.orphan_results.clear();
        for request in self
            .requests
            .values_mut()
            .filter(|request| is_open(request.state.value))
        {
            request.state = Observed::new(RequestStatus::Cancelled, evidence);
        }
    }

    fn observe_collection(&mut self, health: Collection, evidence: &Evidence) -> bool {
        if health == Collection::Healthy && evidence.confidence != Confidence::Confirmed {
            return false;
        }
        let mut evidence = evidence.clone();
        if health != Collection::Healthy {
            self.correlation.generation = self.correlation.generation.saturating_add(1);
            evidence.coverage_generation = self.correlation.generation;
            self.correlation.response_for_prompt = false;
            let uncertain = evidence.uncertain("agent collection coverage changed");
            self.activity = Observed::new(Activity::Unknown, &uncertain);
            for request in self.requests.values_mut() {
                if request.state.value == RequestStatus::Pending {
                    request.state = Observed::new(RequestStatus::Unverified, &uncertain);
                }
            }
        }
        replace(&mut self.collection, health, &evidence)
    }

    fn record_gap(&mut self, unix_ms: u64, reason: &str) {
        let evidence = Evidence {
            source: Source::Collector,
            source_version: env!("CARGO_PKG_VERSION").to_owned(),
            observed_unix_ms: unix_ms,
            provider_unix_ms: None,
            provider_event_id: None,
            confidence: Confidence::Confirmed,
            uncertainty: Some(reason.to_owned()),
            coverage_generation: self.correlation.generation,
        };
        self.observe_collection(Collection::GapDetected, &evidence);
    }

    fn add_attention(&mut self, cause: AttentionCause, request_id: Option<Uuid>, unix_ms: u64) {
        if self.attention.len() == MAX_ATTENTION
            && let Some(id) = self
                .attention
                .values()
                .min_by_key(|attention| {
                    (
                        attention.acknowledged_unix_ms.is_none(),
                        attention.created_unix_ms,
                    )
                })
                .map(|attention| attention.id)
        {
            self.attention.remove(&id);
        }
        let id = Uuid::new_v4();
        self.attention.insert(
            id,
            Attention {
                id,
                cause,
                created_unix_ms: unix_ms,
                acknowledged_unix_ms: None,
                request_id,
            },
        );
    }
}

fn is_open(status: RequestStatus) -> bool {
    matches!(status, RequestStatus::Pending | RequestStatus::Unverified)
}

fn replace<T: PartialEq>(slot: &mut Observed<T>, value: T, evidence: &Evidence) -> bool {
    let next = Observed::new(value, evidence);
    if *slot == next {
        return false;
    }
    *slot = next;
    true
}

impl Observation {
    fn is_bounded(&self) -> bool {
        if self.delivery_id.is_nil()
            || self.evidence.source_version.len() > MAX_EVIDENCE_TEXT_BYTES
            || self
                .evidence
                .uncertainty
                .as_ref()
                .is_some_and(|text| text.len() > MAX_EVIDENCE_TEXT_BYTES)
            || !valid_optional_id(&self.evidence.provider_event_id)
        {
            return false;
        }
        match &self.kind {
            ObservationKind::TurnStarted(turn) => valid_id(turn),
            ObservationKind::WorkStarted(key) | ObservationKind::WorkFinished(key) => {
                valid_optional_id(&key.turn) && valid_id(&key.item)
            }
            ObservationKind::InputRequested { key: Some(key), .. }
            | ObservationKind::InputResolved { key, .. } => {
                valid_optional_id(&key.turn) && valid_id(&key.provider_request_id)
            }
            ObservationKind::Response { response, .. } => valid_optional_id(&response.turn),
            ObservationKind::ContextEnded(EndReason::ProcessExited) => false,
            _ => true,
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_BYTES
}

fn valid_optional_id(id: &Option<String>) -> bool {
    id.as_deref().is_none_or(valid_id)
}
