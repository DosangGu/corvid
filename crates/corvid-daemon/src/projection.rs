use corvid_core::agent::{
    Activity, AgentRun, AttentionCause, Confidence, Evidence, Lifecycle, RequestKind,
    RequestStatus, ResponseOutcome, Source,
};
use corvid_core::layout::{Axis, Layout, Location, Node};
use corvid_protocol::wire::layout_node_state::Body as NodeBody;
use corvid_protocol::wire::snapshot_entity::Body;
use corvid_protocol::wire::{
    AgentActivity, AgentInfo, AgentLifecycle, AgentResponse, AgentState, AttentionReason,
    AttentionState, EvidenceConfidence, EvidenceSource, EvidenceState, InputRequestKind,
    InputRequestStatus, LayoutNodeState, RequestState, SnapshotEntity, SplitAxis, SplitState,
    TabState, ViewSelection, WorkspaceState,
};

use crate::publication::{Group, Projection};

pub(crate) fn entry(body: Body) -> SnapshotEntity {
    SnapshotEntity { body: Some(body) }
}

pub(crate) fn layout(layout: &Layout) -> Projection {
    let workspaces = layout
        .workspaces
        .iter()
        .map(|workspace| {
            let tabs = workspace
                .tabs
                .iter()
                .map(|tab| {
                    let mut nodes = Vec::new();
                    flatten(&tab.root, &mut nodes);
                    TabState {
                        tab_id: tab.id.to_string(),
                        label: tab.label.clone(),
                        nodes,
                    }
                })
                .collect();
            entry(Body::Workspace(WorkspaceState {
                workspace_id: workspace.id.to_string(),
                label: workspace.label.clone(),
                tabs,
            }))
        })
        .collect();
    (Group::Layout, workspaces)
}

fn flatten(node: &Node, nodes: &mut Vec<LayoutNodeState>) -> u32 {
    let index = nodes.len();
    nodes.push(LayoutNodeState::default());
    let body = match node {
        Node::Pane { id, .. } => NodeBody::PaneId(id.to_string()),
        Node::Split { axis, children } => {
            let axis = match axis {
                Axis::Horizontal => SplitAxis::Horizontal,
                Axis::Vertical => SplitAxis::Vertical,
            };
            let children = children.iter().map(|child| flatten(child, nodes)).collect();
            NodeBody::Split(SplitState {
                axis: axis as i32,
                children,
            })
        }
    };
    nodes[index].body = Some(body);
    index as u32
}

pub(crate) fn selection(location: Location) -> ViewSelection {
    ViewSelection {
        workspace_id: location.workspace.to_string(),
        tab_id: location.tab.to_string(),
        pane_id: location.pane.to_string(),
    }
}

pub(crate) fn agent(run: &AgentRun, summary: AgentInfo) -> Vec<SnapshotEntity> {
    let mut entries = Vec::with_capacity(1 + run.requests.len() + run.attention.len());
    let run_id = &summary.agent_run_id;
    for request in run.requests.values() {
        let kind = match request.kind {
            RequestKind::Question => InputRequestKind::Question,
            RequestKind::PlanApproval => InputRequestKind::PlanApproval,
            RequestKind::Permission => InputRequestKind::Permission,
        };
        let status = match request.state.value {
            RequestStatus::Pending => InputRequestStatus::Pending,
            RequestStatus::Resolved => InputRequestStatus::Resolved,
            RequestStatus::Cancelled => InputRequestStatus::Cancelled,
            RequestStatus::Unverified => InputRequestStatus::Unverified,
        };
        entries.push(entry(Body::Request(RequestState {
            agent_run_id: run_id.clone(),
            request_id: request.id.to_string(),
            kind: kind as i32,
            status: status as i32,
            evidence: Some(evidence(&request.state.evidence)),
        })));
    }
    for attention in run.attention.values() {
        let reason = match attention.cause {
            AttentionCause::Input => AttentionReason::Input,
            AttentionCause::Response => AttentionReason::Response,
            AttentionCause::Failure => AttentionReason::Failure,
        };
        entries.push(entry(Body::Attention(AttentionState {
            agent_run_id: run_id.clone(),
            attention_id: attention.id.to_string(),
            reason: reason as i32,
            created_unix_ms: attention.created_unix_ms,
            acknowledged_unix_ms: attention.acknowledged_unix_ms,
            request_id: attention
                .request_id
                .filter(|id| run.requests.contains_key(id))
                .map(|id| id.to_string()),
        })));
    }
    entries.push(entry(Body::Agent(Box::new(agent_state(run, summary)))));
    entries
}

fn agent_state(run: &AgentRun, summary: AgentInfo) -> AgentState {
    let lifecycle = match run.lifecycle.value {
        Lifecycle::Unknown => AgentLifecycle::Unknown,
        Lifecycle::Live => AgentLifecycle::Live,
        Lifecycle::Ended(_) => AgentLifecycle::Ended,
    };
    let activity = match run.activity.value {
        Activity::Unknown => AgentActivity::Unknown,
        Activity::Working => AgentActivity::Working,
        Activity::AwaitingPrompt => AgentActivity::AwaitingPrompt,
        Activity::Interrupted => AgentActivity::Interrupted,
        Activity::Error => AgentActivity::Error,
    };
    let response = match run
        .recent_response
        .as_ref()
        .map(|response| response.value.outcome)
    {
        Some(ResponseOutcome::Observed) => AgentResponse::Observed,
        Some(ResponseOutcome::Interrupted) => AgentResponse::Interrupted,
        Some(ResponseOutcome::Failed) => AgentResponse::Failed,
        None => AgentResponse::Unspecified,
    };
    AgentState {
        created_sequence: None,
        summary: Some(summary),
        lifecycle: lifecycle as i32,
        activity: activity as i32,
        lifecycle_evidence: Some(evidence(&run.lifecycle.evidence)),
        process_evidence: Some(evidence(&run.process.evidence)),
        activity_evidence: Some(evidence(&run.activity.evidence)),
        collection_evidence: Some(evidence(&run.collection.evidence)),
        response_evidence: run
            .recent_response
            .as_ref()
            .map(|response| evidence(&response.evidence)),
        active_turns: run.active_turns.len() as u32,
        active_work_items: run.work.len() as u32,
        response: response as i32,
        parent_run_id: run.identity.parent_run_id.map(|id| id.to_string()),
    }
}

fn evidence(value: &Evidence) -> EvidenceState {
    let source = match value.source {
        Source::Launch => EvidenceSource::Launch,
        Source::ProviderHook => EvidenceSource::ProviderHook,
        Source::ProviderNotify => EvidenceSource::ProviderNotify,
        Source::ProcessProbe => EvidenceSource::ProcessProbe,
        Source::ChildExit => EvidenceSource::ChildExit,
        Source::Collector => EvidenceSource::Collector,
    };
    let confidence = match value.confidence {
        Confidence::Confirmed => EvidenceConfidence::Confirmed,
        Confidence::Inferred => EvidenceConfidence::Inferred,
        Confidence::Unknown => EvidenceConfidence::Unknown,
    };
    EvidenceState {
        source: source as i32,
        source_version: value.source_version.clone(),
        observed_unix_ms: value.observed_unix_ms,
        provider_unix_ms: value.provider_unix_ms,
        confidence: confidence as i32,
        uncertainty: value.uncertainty.clone(),
        coverage_generation: value.coverage_generation,
    }
}
