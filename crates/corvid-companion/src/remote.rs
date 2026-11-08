use std::collections::BTreeMap;

use corvid_protocol::ProtocolFault;
use corvid_protocol::snapshot::SynchronizedSnapshot;
use corvid_protocol::wire::snapshot_entity::Body;
use corvid_protocol::wire::{
    AgentBadge, AgentLifecycle, AgentProcessState, AgentState, InputRequestStatus, PresentedRun,
    PresentedRunKey, RunAvailability,
};
use uuid::Uuid;

use crate::{RunKey, RunObservation};

pub(crate) struct Remote {
    pub(crate) epoch: Uuid,
    pub(crate) sequence: u64,
    pub(crate) bindings: BTreeMap<Uuid, u64>,
    pub(crate) runs: BTreeMap<RunKey, ObservedRun>,
}

pub(crate) struct ObservedRun {
    pub(crate) observation: RunObservation,
    pub(crate) display: PresentedRun,
    latest_alert: Option<(u64, String)>,
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
        };
        for entry in snapshot.entities() {
            match &entry.body {
                Some(Body::Agent(agent)) if agent.parent_run_id.is_none() => {
                    let summary = agent.summary.as_ref().ok_or_else(invalid)?;
                    let key = RunKey {
                        host,
                        epoch,
                        run: parse_id(&summary.agent_run_id)?,
                    };
                    remote.runs.insert(key, ObservedRun::new(key, agent));
                }
                Some(Body::Client(client)) if client.selection.is_some() => {
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
        for entry in snapshot.entities() {
            match &entry.body {
                Some(Body::Attention(alert)) if alert.acknowledged_unix_ms.is_none() => {
                    let key = RunKey {
                        host,
                        epoch,
                        run: parse_id(&alert.agent_run_id)?,
                    };
                    if let Some(run) = remote.runs.get_mut(&key) {
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
                        if request.status == InputRequestStatus::Pending as i32
                            && run.display.last_badge == AgentBadge::NeedsInput as i32
                        {
                            run.display.confidence = evidence.confidence;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(remote)
    }
}

impl ObservedRun {
    fn new(key: RunKey, agent: &AgentState) -> Self {
        let summary = agent.summary.as_ref().expect("validated agent summary");
        let ended = agent.lifecycle == AgentLifecycle::Ended as i32
            || summary.process == AgentProcessState::Exited as i32;
        let navigable = summary.terminal_available && !ended;
        let eligible = navigable
            && (agent.lifecycle == AgentLifecycle::Live as i32
                || summary.process == AgentProcessState::Running as i32);
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
                navigable,
                eligible,
                ended,
            },
            display: PresentedRun {
                key: Some(key.into()),
                availability: availability as i32,
                last_badge: summary.badge,
                provider: summary.provider,
                confidence: evidence.map_or(0, |evidence| evidence.confidence),
                collection: summary.collection,
                pending_requests: summary.pending_requests,
                unverified_requests: summary.unverified_requests,
                last_response_unix_ms: summary.response_observed_unix_ms,
                last_observed_unix_ms,
                ..Default::default()
            },
            latest_alert: None,
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
