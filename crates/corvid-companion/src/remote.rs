use std::collections::{BTreeMap, BTreeSet};

use corvid_protocol::ProtocolFault;
use corvid_protocol::snapshot::SynchronizedSnapshot;
use corvid_protocol::wire::layout_node_state::Body as NodeBody;
use corvid_protocol::wire::snapshot_entity::Body;
use corvid_protocol::wire::{
    AgentBadge, AgentLifecycle, AgentProcessState, AgentState, AttentionReason, EvidenceConfidence,
    InputRequestStatus, PresentedAttention, PresentedRun, PresentedRunKey, PromptObservation,
    RequestState, RunAvailability,
};
use uuid::Uuid;

use crate::{RunKey, RunObservation};

#[cfg(test)]
mod tests;

pub(crate) struct Remote {
    pub(crate) epoch: Uuid,
    pub(crate) sequence: u64,
    pub(crate) bindings: BTreeMap<Uuid, u64>,
    pub(crate) runs: BTreeMap<RunKey, ObservedRun>,
    pub(crate) panes: BTreeSet<Uuid>,
    pub(crate) attention: Vec<PresentedAttention>,
    pub(crate) omitted_attention: u32,
}

pub(crate) struct ObservedRun {
    pub(crate) observation: RunObservation,
    pub(crate) display: PresentedRun,
    pub(crate) process_exited: bool,
    latest_alert: Option<(u64, String)>,
    latest_result: Option<(u64, bool)>,
}

impl Remote {
    pub(crate) fn read(
        cache: &SynchronizedSnapshot,
        controller: &str,
    ) -> Result<Self, ProtocolFault> {
        let host = parse_id(&cache.session().host_id)?;
        let epoch = parse_id(&cache.session().server_epoch)?;
        let snapshot = cache.snapshot();
        let mut remote = Self {
            epoch,
            sequence: snapshot.sequence(),
            bindings: BTreeMap::new(),
            runs: BTreeMap::new(),
            panes: BTreeSet::new(),
            attention: Vec::new(),
            omitted_attention: 0,
        };
        let mut pane_titles = BTreeMap::new();
        for entry in snapshot.entities() {
            match &entry.body {
                Some(Body::Workspace(workspace)) => {
                    for tab in &workspace.tabs {
                        for node in &tab.nodes {
                            if let Some(NodeBody::PaneId(pane)) = &node.body {
                                let title = if tab.nodes.len() == 1 {
                                    tab.manual_name.as_deref().unwrap_or(&node.title)
                                } else {
                                    &node.title
                                };
                                pane_titles.insert(parse_id(pane)?, title.trim());
                            }
                        }
                    }
                }
                Some(Body::Agent(agent)) if agent.parent_run_id.is_none() => {
                    let summary = agent.summary.as_ref().ok_or_else(invalid)?;
                    let key = RunKey {
                        host,
                        epoch,
                        run: parse_id(&summary.agent_run_id)?,
                    };
                    remote.runs.insert(key, ObservedRun::new(key, agent));
                }
                Some(Body::Client(client)) if client.active_selection().is_some() => {
                    if let Some(binding) = client
                        .bindings
                        .iter()
                        .find(|binding| binding.controller_id == controller)
                    {
                        remote
                            .bindings
                            .insert(parse_id(&client.client_id)?, binding.binding_revision);
                    }
                }
                _ => {}
            }
        }
        remote.panes.extend(pane_titles.keys().copied());
        for run in remote.runs.values_mut() {
            let title = run
                .observation
                .pane
                .and_then(|pane| pane_titles.get(&pane).copied());
            run.observation.eligible = title.is_some() && !run.process_exited;
            run.display.session_title = title.filter(|title| !title.is_empty()).map(str::to_owned);
        }
        let requests: BTreeMap<_, _> = snapshot
            .entities()
            .filter_map(|entry| match &entry.body {
                Some(Body::Request(request)) => Some((
                    (request.agent_run_id.as_str(), request.request_id.as_str()),
                    request,
                )),
                _ => None,
            })
            .collect();
        for entry in snapshot.entities() {
            match &entry.body {
                Some(Body::Attention(alert)) => {
                    let key = RunKey {
                        host,
                        epoch,
                        run: parse_id(&alert.agent_run_id)?,
                    };
                    if let Some(run) = remote.runs.get_mut(&key) {
                        let request = alert
                            .request_id
                            .as_deref()
                            .and_then(|id| requests.get(&(alert.agent_run_id.as_str(), id)));
                        match AttentionReason::try_from(alert.reason) {
                            Ok(AttentionReason::Input)
                                if request.is_some_and(|request| pending(request)) =>
                            {
                                run.note_pending_since(alert.created_unix_ms);
                            }
                            Ok(AttentionReason::Response | AttentionReason::Failure) => {
                                run.latest_result = run.latest_result.max(Some((
                                    alert.created_unix_ms,
                                    alert.acknowledged_unix_ms.is_some(),
                                )));
                            }
                            _ => {}
                        }
                        if alert.acknowledged_unix_ms.is_some() {
                            continue;
                        }
                        remote.attention.push(PresentedAttention {
                            run: Some(key.into()),
                            attention_id: alert.attention_id.clone(),
                            reason: alert.reason,
                            created_unix_ms: alert.created_unix_ms,
                            provider: run.display.provider,
                            request_status: request.map(|request| request.status),
                            request_kind: request.map(|request| request.kind),
                        });
                        run.display.unseen_attention += 1;
                        let latest = (alert.created_unix_ms, alert.attention_id.clone());
                        if run.latest_alert.as_ref().is_none_or(|old| *old < latest) {
                            run.display.latest_unseen_attention_id = Some(latest.1.clone());
                            run.latest_alert = Some(latest);
                        }
                    }
                }
                Some(Body::Request(request)) => {
                    let key = RunKey {
                        host,
                        epoch,
                        run: parse_id(&request.agent_run_id)?,
                    };
                    if let Some(run) = remote.runs.get_mut(&key)
                        && let Some(evidence) = &request.evidence
                    {
                        let time = evidence.observed_unix_ms;
                        run.display.last_observed_unix_ms = run
                            .display
                            .last_observed_unix_ms
                            .max((time != 0).then_some(time));
                        if pending(request) {
                            run.note_pending_since(time);
                            run.display.confidence = evidence.confidence;
                        }
                    }
                }
                _ => {}
            }
        }
        remote.attention.sort_unstable_by(|a, b| {
            (b.created_unix_ms, &b.attention_id).cmp(&(a.created_unix_ms, &a.attention_id))
        });
        remote.omitted_attention = remote
            .attention
            .len()
            .saturating_sub(crate::PRESENTATION_ATTENTION_LIMIT)
            as u32;
        remote
            .attention
            .truncate(crate::PRESENTATION_ATTENTION_LIMIT);
        for run in remote.runs.values_mut() {
            let active = run.display.last_badge == AgentBadge::Working as i32
                || PromptObservation::try_from(run.display.prompt_observation).is_ok_and(
                    |observation| {
                        observation == PromptObservation::Submitted
                            || observation.confirms_execution()
                    },
                );
            if !active {
                run.display.new_result_unix_ms = run
                    .latest_result
                    .filter(|(time, acknowledged)| *time != 0 && !acknowledged)
                    .map(|(time, _)| time);
            }
        }
        Ok(remote)
    }
}

fn pending(request: &RequestState) -> bool {
    request.status == InputRequestStatus::Pending as i32
        && request
            .evidence
            .as_ref()
            .is_some_and(|evidence| evidence.confidence == EvidenceConfidence::Confirmed as i32)
}

impl ObservedRun {
    #[cfg(test)]
    pub(crate) fn for_test(observation: RunObservation, display: PresentedRun) -> Self {
        Self {
            observation,
            display,
            process_exited: false,
            latest_alert: None,
            latest_result: None,
        }
    }

    fn note_pending_since(&mut self, time: u64) {
        if time != 0 {
            let since = &mut self.display.pending_since_unix_ms;
            *since = Some(since.map_or(time, |old| old.min(time)));
        }
    }

    fn new(key: RunKey, agent: &AgentState) -> Self {
        let summary = agent.summary.as_ref().expect("validated agent summary");
        let process_exited = summary.process == AgentProcessState::Exited as i32;
        let ended = agent.lifecycle == AgentLifecycle::Ended as i32 || process_exited;
        let navigable = summary.terminal_available && !ended;
        let availability = if navigable {
            RunAvailability::Available
        } else {
            RunAvailability::CheckNeeded
        };
        let evidence = if summary.badge == AgentBadge::ResponseObserved as i32 {
            agent.response_evidence.as_ref()
        } else {
            agent.activity_evidence.as_ref()
        };
        let last_observed_unix_ms = [
            &agent.lifecycle_evidence,
            &agent.process_evidence,
            &agent.activity_evidence,
            &agent.collection_evidence,
            &agent.response_evidence,
        ]
        .into_iter()
        .filter_map(|item| item.as_ref().map(|evidence| evidence.observed_unix_ms))
        .max()
        .filter(|time| *time != 0);
        Self {
            observation: RunObservation {
                created_sequence: agent.created_sequence,
                pane: summary
                    .location
                    .as_ref()
                    .and_then(|location| Uuid::parse_str(&location.pane_id).ok()),
                navigable,
                eligible: false,
                ended,
            },
            display: PresentedRun {
                key: Some(key.into()),
                availability: availability as i32,
                last_badge: summary.badge,
                prompt_observation: agent.prompt_observation,
                prompt_observed_unix_ms: summary.prompt_observed_unix_ms,
                provider: summary.provider,
                confidence: evidence.map_or(0, |evidence| evidence.confidence),
                collection: summary.collection,
                pending_requests: summary.pending_requests,
                unverified_requests: summary.unverified_requests,
                last_response_unix_ms: summary.response_observed_unix_ms,
                last_observed_unix_ms,
                ..Default::default()
            },
            process_exited,
            latest_alert: None,
            latest_result: None,
        }
    }
}

impl From<RunKey> for PresentedRunKey {
    fn from(key: RunKey) -> Self {
        Self {
            host_id: key.host.to_string(),
            remote_epoch: key.epoch.to_string(),
            agent_run_id: key.run.to_string(),
        }
    }
}

impl TryFrom<&PresentedRunKey> for RunKey {
    type Error = ProtocolFault;

    fn try_from(key: &PresentedRunKey) -> Result<Self, Self::Error> {
        Ok(Self {
            host: parse_id(&key.host_id)?,
            epoch: parse_id(&key.remote_epoch)?,
            run: parse_id(&key.agent_run_id)?,
        })
    }
}

pub(crate) fn parse_id(value: &str) -> Result<Uuid, ProtocolFault> {
    Uuid::parse_str(value)
        .ok()
        .filter(|id| !id.is_nil())
        .ok_or_else(invalid)
}

pub(crate) fn invalid() -> ProtocolFault {
    ProtocolFault::new(
        "InvalidMessage",
        "invalid companion request or remote identity",
    )
}
