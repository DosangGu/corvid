//! Provider-independent state derived from authenticated agent observations.

mod reducer;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use reducer::Correlation;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Claude,
    Codex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Confirmed,
    Inferred,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Launch,
    ProviderHook,
    ProviderNotify,
    ProcessProbe,
    ChildExit,
    Collector,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub source: Source,
    pub source_version: String,
    pub observed_unix_ms: u64,
    pub provider_unix_ms: Option<u64>,
    pub provider_event_id: Option<String>,
    pub confidence: Confidence,
    pub uncertainty: Option<String>,
    pub coverage_generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observed<T> {
    pub value: T,
    pub evidence: Evidence,
}

impl<T> Observed<T> {
    fn new(value: T, evidence: &Evidence) -> Self {
        Self {
            value,
            evidence: evidence.clone(),
        }
    }
}

impl Evidence {
    fn uncertain(&self, reason: &str) -> Self {
        let mut evidence = self.clone();
        evidence.confidence = Confidence::Unknown;
        evidence
            .uncertainty
            .get_or_insert_with(|| reason.to_owned());
        evidence
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    ProcessExited,
    ContextEnded,
    ContextReplaced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Live,
    Ended(EndReason),
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessLiveness {
    Running,
    Exited { code: Option<u32> },
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Working,
    AwaitingPrompt,
    Interrupted,
    Error,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Collection {
    NotRegistered,
    Healthy,
    Degraded,
    GapDetected,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    Question,
    PlanApproval,
    Permission,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Pending,
    Resolved,
    Cancelled,
    Unverified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    Resolved,
    Cancelled,
}

impl From<Resolution> for RequestStatus {
    fn from(value: Resolution) -> Self {
        match value {
            Resolution::Resolved => Self::Resolved,
            Resolution::Cancelled => Self::Cancelled,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RequestKey {
    pub turn: Option<String>,
    pub provider_request_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputRequest {
    pub id: Uuid,
    pub key: Option<RequestKey>,
    pub kind: RequestKind,
    pub state: Observed<RequestStatus>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseOutcome {
    Observed,
    Interrupted,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub turn: Option<String>,
    pub outcome: ResponseOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionCause {
    Input,
    Response,
    Failure,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attention {
    pub id: Uuid,
    pub cause: AttentionCause,
    pub created_unix_ms: u64,
    pub acknowledged_unix_ms: Option<u64>,
    pub request_id: Option<Uuid>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WorkKey {
    pub turn: Option<String>,
    pub item: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservationKind {
    ContextObserved,
    TurnStarted(String),
    /// Another interactive submission was observed without a result for the prior prompt.
    InteractivePromptSubmitted(String),
    WorkStarted(WorkKey),
    WorkFinished(WorkKey),
    /// A key is present only when the adapter can prove its correlation scope.
    InputRequested {
        key: Option<RequestKey>,
        kind: RequestKind,
    },
    InputResolved {
        key: RequestKey,
        resolution: Resolution,
    },
    InputBatchFinished {
        turn: String,
        tools: Vec<String>,
    },
    Response {
        response: Response,
        final_boundary: bool,
    },
    PromptReady,
    ContextEnded(EndReason),
    Process(ProcessLiveness),
    Collection(Collection),
}

pub struct Observation {
    pub run_id: Uuid,
    pub delivery_id: Uuid,
    pub evidence: Evidence,
    pub kind: ObservationKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    Ended,
    NeedsInput(usize),
    Unknown,
    Interrupted,
    Error,
    Working,
    ResponseObserved,
    Ready,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunIdentity {
    pub id: Uuid,
    pub terminal_id: Uuid,
    pub launch_association: Uuid,
    pub provider: Provider,
    pub provider_session: Option<String>,
    pub parent_run_id: Option<Uuid>,
    pub actor: Option<String>,
}

pub struct AgentRun {
    pub identity: RunIdentity,
    pub lifecycle: Observed<Lifecycle>,
    pub process: Observed<ProcessLiveness>,
    pub activity: Observed<Activity>,
    pub collection: Observed<Collection>,
    pub recent_response: Option<Observed<Response>>,
    pub requests: BTreeMap<Uuid, InputRequest>,
    pub attention: BTreeMap<Uuid, Attention>,
    pub active_turns: BTreeMap<String, Evidence>,
    pub work: BTreeMap<WorkKey, Evidence>,
    correlation: Correlation,
}

impl AgentRun {
    pub fn new(identity: RunIdentity, mut evidence: Evidence) -> Self {
        evidence.coverage_generation = 0;
        Self {
            identity,
            lifecycle: Observed::new(Lifecycle::Unknown, &evidence),
            process: Observed::new(ProcessLiveness::Unknown, &evidence),
            activity: Observed::new(Activity::Unknown, &evidence),
            collection: Observed::new(Collection::NotRegistered, &evidence),
            recent_response: None,
            requests: BTreeMap::new(),
            attention: BTreeMap::new(),
            active_turns: BTreeMap::new(),
            work: BTreeMap::new(),
            correlation: Correlation::default(),
        }
    }

    pub fn badge(&self) -> Badge {
        if matches!(self.lifecycle.value, Lifecycle::Ended(_)) {
            return Badge::Ended;
        }
        let pending = self
            .requests
            .values()
            .filter(|request| {
                request.state.value == RequestStatus::Pending
                    && request.state.evidence.confidence == Confidence::Confirmed
            })
            .count();
        if pending > 0 {
            return Badge::NeedsInput(pending);
        }
        if self.lifecycle.value == Lifecycle::Unknown
            || self.collection.value != Collection::Healthy
            || self.activity.evidence.confidence != Confidence::Confirmed
            || self
                .requests
                .values()
                .any(|request| request.state.value == RequestStatus::Unverified)
        {
            return Badge::Unknown;
        }
        match self.activity.value {
            Activity::Working => Badge::Working,
            Activity::Interrupted => Badge::Interrupted,
            Activity::Error => Badge::Error,
            Activity::AwaitingPrompt => {
                if self.correlation.response_for_prompt {
                    Badge::ResponseObserved
                } else {
                    Badge::Ready
                }
            }
            Activity::Unknown => Badge::Unknown,
        }
    }

    pub fn acknowledge(&mut self, attention_id: Uuid, unix_ms: u64) -> bool {
        let Some(attention) = self.attention.get_mut(&attention_id) else {
            return false;
        };
        if attention.acknowledged_unix_ms.is_some() {
            return false;
        }
        attention.acknowledged_unix_ms = Some(unix_ms);
        true
    }
}
