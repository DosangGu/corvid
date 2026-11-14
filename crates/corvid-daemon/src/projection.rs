use corvid_core::agent::{
    Activity, Attention, AttentionCause, Confidence, EndReason, Evidence, Lifecycle,
    ProcessLiveness, Provider, RequestKind, RequestStatus, Source,
};
use corvid_core::agent::{AgentRun, ResponseOutcome};
use corvid_core::layout::{Axis, Layout, Location, Node};
use corvid_protocol::wire::layout_node_state::Body as NodeBody;
use corvid_protocol::wire::snapshot_entity::Body;
use corvid_protocol::wire::{
    AgentActivity, AgentEndReason, AgentLifecycle, AgentProcessState, AgentProvider,
    AttentionReason, AttentionState, EvidenceConfidence, EvidenceSource, EvidenceState,
    InputRequestKind, InputRequestStatus, LayoutNodeState, SnapshotEntity, SplitAxis, SplitState,
    TabState, ViewSelection, WorkspaceState,
};
use corvid_protocol::wire::{AgentInfo, AgentResponse, AgentState, RequestState};

use crate::publication::{Group, Projection};

pub(crate) fn entry(body: Body) -> SnapshotEntity {
    SnapshotEntity { body: Some(body) }
}

pub(crate) fn layout(layout: &Layout) -> Projection {
    named_layout(layout, &std::collections::HashMap::new())
}

pub(crate) fn named_layout(
    layout: &Layout,
    titles: &std::collections::HashMap<uuid::Uuid, String>,
) -> Projection {
    let workspaces = layout
        .workspaces
        .iter()
        .map(|workspace| {
            let tabs = workspace
                .tabs
                .iter()
                .enumerate()
                .map(|(index, tab)| {
                    let mut nodes = Vec::new();
                    flatten(&tab.root, &mut nodes, titles, &mut 0);
                    TabState {
                        tab_id: tab.id.to_string(),
                        label: tab.display_name(index, titles),
                        manual_name: tab.manual_name.clone(),
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

fn flatten(
    node: &Node,
    nodes: &mut Vec<LayoutNodeState>,
    titles: &std::collections::HashMap<uuid::Uuid, String>,
    ordinal: &mut usize,
) -> u32 {
    let index = nodes.len();
    nodes.push(LayoutNodeState::default());
    let body = match node {
        Node::Pane { id, .. } => {
            nodes[index].title = corvid_core::layout::pane_name(*id, *ordinal, titles);
            *ordinal += 1;
            NodeBody::PaneId(id.to_string())
        }
        Node::Split { axis, children } => {
            let axis = match axis {
                Axis::Horizontal => SplitAxis::Horizontal,
                Axis::Vertical => SplitAxis::Vertical,
            };
            let children = children
                .iter()
                .map(|child| flatten(child, nodes, titles, ordinal))
                .collect();
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
        entries.push(entry(Body::Request(RequestState {
            agent_run_id: run_id.clone(),
            request_id: request.id.to_string(),
            kind: request_kind(request.kind) as i32,
            status: request_status(request.state.value) as i32,
            evidence: Some(evidence(&request.state.evidence)),
        })));
    }
    for record in run.attention.values() {
        let mut state = attention(record, run_id);
        if record
            .request_id
            .is_some_and(|id| !run.requests.contains_key(&id))
        {
            state.request_id = None;
        }
        entries.push(entry(Body::Attention(state)));
    }
    entries.push(entry(Body::Agent(Box::new(agent_state(run, summary)))));
    entries
}

pub(crate) fn lifecycle(value: Lifecycle) -> AgentLifecycle {
    match value {
        Lifecycle::Unknown => AgentLifecycle::Unknown,
        Lifecycle::Live => AgentLifecycle::Live,
        Lifecycle::Ended(_) => AgentLifecycle::Ended,
    }
}

pub(crate) fn activity(value: Activity) -> AgentActivity {
    match value {
        Activity::Unknown => AgentActivity::Unknown,
        Activity::Working => AgentActivity::Working,
        Activity::AwaitingPrompt => AgentActivity::AwaitingPrompt,
        Activity::Interrupted => AgentActivity::Interrupted,
        Activity::Error => AgentActivity::Error,
    }
}

pub(crate) fn provider(value: Provider) -> AgentProvider {
    match value {
        Provider::Claude => AgentProvider::Claude,
        Provider::Codex => AgentProvider::Codex,
    }
}

pub(crate) fn process(value: ProcessLiveness) -> (AgentProcessState, Option<u32>) {
    match value {
        ProcessLiveness::Running => (AgentProcessState::Running, None),
        ProcessLiveness::Exited { code } => (AgentProcessState::Exited, code),
        ProcessLiveness::Unknown => (AgentProcessState::Unknown, None),
    }
}

pub(crate) fn end_reason(value: Lifecycle) -> AgentEndReason {
    match value {
        Lifecycle::Ended(EndReason::ProcessExited) => AgentEndReason::ProcessExited,
        Lifecycle::Ended(EndReason::ContextEnded) => AgentEndReason::ContextEnded,
        Lifecycle::Ended(EndReason::ContextReplaced) => AgentEndReason::ContextReplaced,
        _ => AgentEndReason::Unspecified,
    }
}

pub(crate) fn request_kind(value: RequestKind) -> InputRequestKind {
    match value {
        RequestKind::Question => InputRequestKind::Question,
        RequestKind::PlanApproval => InputRequestKind::PlanApproval,
        RequestKind::Permission => InputRequestKind::Permission,
    }
}

pub(crate) fn request_status(value: RequestStatus) -> InputRequestStatus {
    match value {
        RequestStatus::Pending => InputRequestStatus::Pending,
        RequestStatus::Resolved => InputRequestStatus::Resolved,
        RequestStatus::Cancelled => InputRequestStatus::Cancelled,
        RequestStatus::Unverified => InputRequestStatus::Unverified,
    }
}

pub(crate) fn attention(record: &Attention, run: &str) -> AttentionState {
    let reason = match record.cause {
        AttentionCause::Input => AttentionReason::Input,
        AttentionCause::Response => AttentionReason::Response,
        AttentionCause::Failure => AttentionReason::Failure,
    };
    AttentionState {
        agent_run_id: run.to_owned(),
        attention_id: record.id.to_string(),
        reason: reason as i32,
        created_unix_ms: record.created_unix_ms,
        acknowledged_unix_ms: record.acknowledged_unix_ms,
        request_id: record.request_id.map(|id| id.to_string()),
    }
}

fn agent_state(run: &AgentRun, summary: AgentInfo) -> AgentState {
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
        lifecycle: lifecycle(run.lifecycle.value) as i32,
        activity: activity(run.activity.value) as i32,
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

pub(crate) fn evidence(value: &Evidence) -> EvidenceState {
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
