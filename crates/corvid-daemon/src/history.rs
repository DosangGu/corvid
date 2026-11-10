use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

#[cfg(target_os = "linux")]
use corvid_core::agent::AgentRun;
use corvid_core::agent::{
    Activity, Attention, Evidence, Lifecycle, ProcessLiveness, Provider, RequestKind, RequestStatus,
};
use corvid_core::layout::Location;
use corvid_protocol::snapshot::MAX_HISTORICAL_RUNS;
use corvid_protocol::wire::{HistoricalRequest, HistoricalRun};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::projection;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct History(VecDeque<Arc<RunRecord>>);

#[derive(Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct RunRecord {
    epoch: Uuid,
    run: Uuid,
    terminal: Uuid,
    location: Location,
    provider: Provider,
    session: Option<String>,
    parent: Option<Uuid>,
    lifecycle: Lifecycle,
    process: ProcessLiveness,
    activity: Activity,
    evidence: Evidence,
    requests: Vec<RequestRecord>,
    attention: Vec<Attention>,
}

#[derive(Deserialize, PartialEq, Eq, Serialize)]
struct RequestRecord {
    id: Uuid,
    kind: RequestKind,
    status: RequestStatus,
}

impl History {
    pub(crate) fn record(&mut self, record: RunRecord) -> bool {
        if let Some(index) = self.0.iter().position(|old| old.run == record.run) {
            if *self.0[index] == record {
                return false;
            }
            self.0.remove(index);
        } else if self.0.len() == MAX_HISTORICAL_RUNS {
            let index = self
                .0
                .iter()
                .position(|old| !old.active_in(record.epoch))
                .unwrap_or(0);
            self.0.remove(index);
        }
        self.0.push_back(Arc::new(record));
        true
    }

    pub(crate) fn validate(&self) -> bool {
        self.0.len() <= MAX_HISTORICAL_RUNS
            && distinct_ids(self.0.iter().map(|record| record.run))
            && self.0.iter().all(|record| record.validate())
    }

    pub(crate) fn wire(&self) -> Vec<HistoricalRun> {
        self.0.iter().rev().map(|record| record.wire()).collect()
    }
}

impl RunRecord {
    fn active_in(&self, epoch: Uuid) -> bool {
        self.epoch == epoch
            && self.parent.is_none()
            && !matches!(self.lifecycle, Lifecycle::Ended(_))
            && self.process == ProcessLiveness::Running
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn capture(epoch: Uuid, run: &AgentRun, location: Location) -> Self {
        let mut evidence = [
            &run.lifecycle.evidence,
            &run.process.evidence,
            &run.activity.evidence,
            &run.collection.evidence,
        ]
        .into_iter()
        .chain(
            run.recent_response
                .iter()
                .map(|response| &response.evidence),
        )
        .chain(run.requests.values().map(|request| &request.state.evidence))
        .max_by_key(|evidence| evidence.observed_unix_ms)
        .expect("a run has lifecycle evidence")
        .clone();
        evidence.provider_event_id = None;
        evidence.uncertainty = None;

        let requests = run
            .requests
            .values()
            .map(|request| RequestRecord {
                id: request.id,
                kind: request.kind,
                status: request.state.value,
            })
            .collect();
        let attention = run
            .attention
            .values()
            .map(|attention| Attention {
                request_id: attention
                    .request_id
                    .filter(|id| run.requests.contains_key(id)),
                ..attention.clone()
            })
            .collect();

        Self {
            epoch,
            run: run.identity.id,
            terminal: run.identity.terminal_id,
            location,
            provider: run.identity.provider,
            session: run.identity.provider_session.clone(),
            parent: run.identity.parent_run_id,
            lifecycle: run.lifecycle.value,
            process: run.process.value,
            activity: run.activity.value,
            evidence,
            requests,
            attention,
        }
    }

    fn validate(&self) -> bool {
        [
            self.epoch,
            self.terminal,
            self.location.workspace,
            self.location.tab,
            self.location.pane,
        ]
        .into_iter()
        .all(|id| !id.is_nil())
            && self
                .parent
                .is_none_or(|parent| !parent.is_nil() && parent != self.run)
            && self.session.as_deref().is_none_or(valid_label)
            && valid_label(&self.evidence.source_version)
            && self.evidence.provider_event_id.is_none()
            && self.evidence.uncertainty.is_none()
            && self.requests.len() <= 256
            && self.attention.len() <= 256
            && distinct_ids(self.requests.iter().map(|request| request.id))
            && distinct_ids(self.attention.iter().map(|attention| attention.id))
            && self.attention.iter().all(|attention| {
                attention
                    .request_id
                    .is_none_or(|id| self.requests.iter().any(|request| request.id == id))
            })
    }

    fn wire(&self) -> HistoricalRun {
        let run_id = self.run.to_string();
        let (last_process, last_exit_code) = projection::process(self.process);
        HistoricalRun {
            server_epoch: self.epoch.to_string(),
            terminal_id: self.terminal.to_string(),
            last_location: Some(projection::selection(self.location)),
            provider: projection::provider(self.provider) as i32,
            provider_session_id: self.session.clone(),
            parent_run_id: self.parent.map(|id| id.to_string()),
            last_lifecycle: projection::lifecycle(self.lifecycle) as i32,
            last_process: last_process as i32,
            last_activity: projection::activity(self.activity) as i32,
            last_end_reason: projection::end_reason(self.lifecycle) as i32,
            last_evidence: Some(projection::evidence(&self.evidence)),
            requests: self.requests.iter().map(RequestRecord::wire).collect(),
            attention: self
                .attention
                .iter()
                .map(|attention| projection::attention(attention, &run_id))
                .collect(),
            agent_run_id: run_id,
            last_exit_code,
        }
    }
}

impl RequestRecord {
    fn wire(&self) -> HistoricalRequest {
        let status = match self.status {
            RequestStatus::Pending => RequestStatus::Unverified,
            status => status,
        };
        HistoricalRequest {
            request_id: self.id.to_string(),
            kind: projection::request_kind(self.kind) as i32,
            status: projection::request_status(status) as i32,
        }
    }
}

fn distinct_ids(ids: impl Iterator<Item = Uuid>) -> bool {
    let mut seen = BTreeSet::new();
    ids.into_iter().all(|id| !id.is_nil() && seen.insert(id))
}

fn valid_label(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}
