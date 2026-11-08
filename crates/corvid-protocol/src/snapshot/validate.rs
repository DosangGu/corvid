use std::collections::BTreeSet;

use super::EntityKey;
use super::cache::{Entries, resync};
use crate::ProtocolFault;
use crate::session::valid_id;
use crate::wire::layout_node_state::Body as NodeBody;
use crate::wire::snapshot_entity::Body;
use crate::wire::{
    AgentActivity, AgentEndReason, AgentLifecycle, AgentProcessState, AgentProvider, AgentState,
    AttentionReason, ClientState, EntityKind, EvidenceConfidence, EvidenceSource, EvidenceState,
    HistoricalRun, InputRequestKind, InputRequestStatus, LayoutNodeState, SnapshotEntity,
    SplitAxis, TuiInputMode, ViewSelection, WorkspaceState,
};

type Result<T> = std::result::Result<T, ProtocolFault>;

pub(super) fn key(item: &SnapshotEntity) -> Result<EntityKey> {
    let key = EntityKey::from_entity(item).ok_or_else(|| resync("unknown snapshot entity"))?;
    require(
        valid_id(&key.id) && key.agent_run_id.as_deref().is_none_or(valid_id),
        "invalid entity ID",
    )?;
    Ok(key)
}

pub(super) fn all(entries: &Entries) -> Result<()> {
    validate_layout(entries)?;
    let mut writers = BTreeSet::new();
    let mut live_panes = BTreeSet::new();
    for item in entries.values() {
        entity(entries, item)?;
        if let Some(Body::Client(client)) = &item.body {
            for lease in &client.writer_leases {
                require(
                    writers.insert(&lease.workspace_id),
                    "duplicate workspace writer",
                )?;
            }
        }
        if let Some(Body::Terminal(terminal)) = &item.body
            && let Some(location) = &terminal.location
        {
            require(
                live_panes.insert(&location.pane_id),
                "pane has more than one live terminal",
            )?;
        }
    }
    Ok(())
}

pub(super) fn historical_run(record: &HistoricalRun, current_epoch: &str) -> Result<()> {
    require(
        record.server_epoch != current_epoch
            && valid_id(&record.server_epoch)
            && valid_id(&record.agent_run_id)
            && valid_id(&record.terminal_id)
            && record.last_location.as_ref().is_some_and(valid_location)
            && record.provider_session_id.as_deref().is_none_or(valid_id)
            && record
                .parent_run_id
                .as_deref()
                .is_none_or(|id| id != record.agent_run_id && valid_id(id)),
        "invalid historical run scope",
    )?;
    let ended = record.last_lifecycle == AgentLifecycle::Ended as i32;
    require(
        matches!(
            AgentProvider::try_from(record.provider),
            Ok(AgentProvider::Claude | AgentProvider::Codex)
        ) && matches!(AgentLifecycle::try_from(record.last_lifecycle), Ok(value) if value != AgentLifecycle::Unspecified)
            && matches!(AgentProcessState::try_from(record.last_process), Ok(value) if value != AgentProcessState::Unspecified)
            && (record.last_exit_code.is_none()
                || record.last_process == AgentProcessState::Exited as i32)
            && matches!(AgentActivity::try_from(record.last_activity), Ok(value) if value != AgentActivity::Unspecified)
            && matches!(AgentEndReason::try_from(record.last_end_reason), Ok(value) if ended == (value != AgentEndReason::Unspecified))
            && record.last_evidence.as_ref().is_some_and(|value| {
                evidence(Some(value))
                    && EvidenceSource::try_from(value.source).is_ok()
                    && EvidenceConfidence::try_from(value.confidence).is_ok()
            }),
        "invalid historical observation",
    )?;
    require(
        record.requests.len() <= 256 && record.attention.len() <= 256,
        "too many historical observations",
    )?;
    let mut requests = BTreeSet::new();
    for request in &record.requests {
        require(
            valid_id(&request.request_id)
                && requests.insert(&request.request_id)
                && matches!(InputRequestKind::try_from(request.kind), Ok(value) if value != InputRequestKind::Unspecified)
                && matches!(
                    InputRequestStatus::try_from(request.status),
                    Ok(InputRequestStatus::Resolved
                        | InputRequestStatus::Cancelled
                        | InputRequestStatus::Unverified)
                ),
            "invalid historical request",
        )?;
    }
    let mut attention = BTreeSet::new();
    for item in &record.attention {
        require(
            item.agent_run_id == record.agent_run_id
                && valid_id(&item.attention_id)
                && attention.insert(&item.attention_id)
                && matches!(AttentionReason::try_from(item.reason), Ok(value) if value != AttentionReason::Unspecified)
                && item
                    .request_id
                    .as_ref()
                    .is_none_or(|id| requests.contains(id)),
            "invalid historical attention",
        )?;
    }
    Ok(())
}

pub(super) fn entity(entries: &Entries, item: &SnapshotEntity) -> Result<()> {
    match item
        .body
        .as_ref()
        .ok_or_else(|| resync("unknown snapshot entity"))?
    {
        Body::Workspace(_) => Ok(()),
        Body::Terminal(terminal) => {
            let location = terminal
                .location
                .as_ref()
                .ok_or_else(|| resync("terminal location missing"))?;
            require(
                pane_exists(entries, location),
                "terminal location is missing from layout",
            )
        }
        Body::Client(client) => client_state(entries, client),
        Body::Agent(agent) => agent_state(entries, agent),
        Body::Request(request) => {
            require(
                lookup(entries, EntityKind::Agent, &request.agent_run_id, None).is_some(),
                "request owner is missing",
            )?;
            require(
                request.kind != 0 && request.status != 0 && evidence(request.evidence.as_ref()),
                "invalid request state",
            )
        }
        Body::Attention(attention) => {
            require(
                lookup(entries, EntityKind::Agent, &attention.agent_run_id, None).is_some(),
                "attention owner is missing",
            )?;
            require(attention.reason != 0, "invalid attention reason")?;
            if let Some(request) = &attention.request_id {
                require(
                    lookup(
                        entries,
                        EntityKind::Request,
                        request,
                        Some(&attention.agent_run_id),
                    )
                    .is_some(),
                    "attention refers to an unavailable request",
                )?;
            }
            Ok(())
        }
    }
}

fn client_state(entries: &Entries, client: &ClientState) -> Result<()> {
    require(
        client.connection_generation > 0,
        "invalid client generation",
    )?;
    require(client.writer_leases.len() <= 32, "too many writer leases")?;
    require(client.bindings.len() <= 8, "too many client bindings")?;
    if let Some(mode) = client.input_mode {
        require(
            client.terminal_id.is_some()
                && client.keymap.is_some()
                && matches!(
                    TuiInputMode::try_from(mode),
                    Ok(TuiInputMode::Passthrough | TuiInputMode::Command)
                ),
            "invalid client input mode",
        )?;
    }
    let mut controllers = BTreeSet::new();
    for binding in &client.bindings {
        require(
            binding.binding_revision > 0
                && valid_id(&binding.controller_id)
                && controllers.insert(&binding.controller_id),
            "invalid client binding",
        )?;
    }
    for lease in &client.writer_leases {
        require(
            lease.generation > 0 && workspace(entries, &lease.workspace_id).is_some(),
            "invalid workspace lease",
        )?;
    }
    match (
        &client.terminal_id,
        &client.selection,
        client.view_revision,
        &client.viewport,
    ) {
        (None, None, None, None) => Ok(()),
        (Some(terminal), Some(location), Some(revision), Some(viewport)) => require(
            revision > 0
                && (2..=300).contains(&viewport.cols)
                && (2..=120).contains(&viewport.rows)
                && terminal_matches(entries, terminal, location)
                && client
                    .writer_leases
                    .iter()
                    .any(|lease| lease.workspace_id == location.workspace_id),
            "client view refers to an unavailable terminal or lease",
        ),
        _ => Err(resync("incomplete client view")),
    }
}

fn agent_state(entries: &Entries, agent: &AgentState) -> Result<()> {
    let summary = agent
        .summary
        .as_ref()
        .ok_or_else(|| resync("missing agent summary"))?;
    require(valid_id(&summary.terminal_id), "invalid agent terminal ID")?;
    require(
        summary.provider != 0
            && summary.badge != 0
            && summary.collection != 0
            && summary.process != 0
            && agent.lifecycle != 0
            && agent.activity != 0
            && agent.active_turns <= 256
            && agent.active_work_items <= 256,
        "invalid agent observation state",
    )?;
    require(
        summary.provider_session_id.as_deref().is_none_or(valid_id)
            && summary
                .qualified_provider_version
                .as_deref()
                .is_none_or(valid_display),
        "invalid provider identity",
    )?;
    if let Some(adapter) = &summary.adapter {
        require(
            adapter.mode > 0
                && adapter.schema_revision > 0
                && adapter.qualified_events.len() <= 32
                && adapter.correlation.len() <= 16
                && adapter.capabilities.len() <= 32
                && adapter
                    .qualified_events
                    .iter()
                    .chain(&adapter.correlation)
                    .all(|value| *value > 0)
                && adapter.capabilities.iter().all(|capability| {
                    capability.kind > 0
                        && capability.qualification > 0
                        && valid_display(&capability.reason)
                }),
            "invalid provider adapter capabilities",
        )?;
    }
    if let Some(identity) = &summary.process_identity {
        require(
            identity.pid > 0
                && valid_id(&identity.boot_id)
                && identity.foreground_group.is_none_or(|group| group > 0),
            "invalid process identity",
        )?;
    }
    for field in [
        &agent.lifecycle_evidence,
        &agent.process_evidence,
        &agent.activity_evidence,
        &agent.collection_evidence,
    ] {
        require(evidence(field.as_ref()), "missing agent evidence")?;
    }
    if agent.response != 0 {
        require(
            evidence(agent.response_evidence.as_ref()),
            "missing response evidence",
        )?;
    }
    if let Some(parent) = &agent.parent_run_id {
        require(
            parent != &summary.agent_run_id
                && lookup(entries, EntityKind::Agent, parent, None).is_some(),
            "agent parent is missing",
        )?;
    }
    require(
        match (&summary.parent_run_id, &summary.actor_id) {
            (None, None) => true,
            (Some(parent), Some(actor)) => {
                agent.parent_run_id.as_ref() == Some(parent)
                    && valid_id(actor)
                    && !summary.terminal_available
            }
            _ => false,
        },
        "invalid child agent identity",
    )?;
    if let Some(location) = &summary.location {
        require(valid_location(location), "invalid agent location")?;
    }
    if summary.terminal_available {
        require(
            summary
                .location
                .as_ref()
                .is_some_and(|location| terminal_matches(entries, &summary.terminal_id, location)),
            "agent location refers to an unavailable terminal",
        )?;
    }
    Ok(())
}

fn validate_layout(entries: &Entries) -> Result<()> {
    let mut ids = BTreeSet::new();
    let mut workspaces = 0;
    let mut tabs = 0;
    let mut nodes = 0;
    for item in entries.values() {
        let Some(Body::Workspace(workspace)) = &item.body else {
            continue;
        };
        workspaces += 1;
        tabs += workspace.tabs.len();
        require(
            workspaces <= 32 && tabs <= 128 && !workspace.tabs.is_empty(),
            "layout exceeds its bounds",
        )?;
        require(
            ids.insert(workspace.workspace_id.as_str()) && valid_display(&workspace.label),
            "invalid workspace",
        )?;
        for tab in &workspace.tabs {
            require(
                valid_id(&tab.tab_id)
                    && ids.insert(tab.tab_id.as_str())
                    && valid_display(&tab.label),
                "invalid tab",
            )?;
            nodes += tab.nodes.len();
            require(
                nodes <= 1024 && !tab.nodes.is_empty(),
                "invalid pane tree size",
            )?;
            validate_tree(&tab.nodes, &mut ids)?;
        }
    }
    require(workspaces > 0, "snapshot has no layout")
}

fn validate_tree<'a>(nodes: &'a [LayoutNodeState], ids: &mut BTreeSet<&'a str>) -> Result<()> {
    let mut depths = vec![0; nodes.len()];
    depths[0] = 1;
    for (index, node) in nodes.iter().enumerate() {
        let depth = depths[index];
        require((1..=32).contains(&depth), "invalid pane tree depth")?;
        match &node.body {
            Some(NodeBody::PaneId(pane)) => require(
                valid_id(pane) && ids.insert(pane.as_str()),
                "duplicate or invalid pane",
            )?,
            Some(NodeBody::Split(split)) => {
                require(
                    matches!(
                        SplitAxis::try_from(split.axis),
                        Ok(SplitAxis::Horizontal | SplitAxis::Vertical)
                    ) && split.children.len() >= 2,
                    "invalid pane split",
                )?;
                for child in &split.children {
                    let child = *child as usize;
                    require(
                        child > index && depths.get(child) == Some(&0),
                        "invalid pane tree edge",
                    )?;
                    depths[child] = depth + 1;
                }
            }
            None => return Err(resync("unknown pane node")),
        }
    }
    Ok(())
}

fn workspace<'a>(entries: &'a Entries, id: &str) -> Option<&'a WorkspaceState> {
    match lookup(entries, EntityKind::Workspace, id, None)? {
        Body::Workspace(workspace) => Some(workspace),
        _ => None,
    }
}

fn pane_exists(entries: &Entries, location: &ViewSelection) -> bool {
    workspace(entries, &location.workspace_id)
        .and_then(|workspace| workspace.tabs.iter().find(|tab| tab.tab_id == location.tab_id))
        .is_some_and(|tab| tab.nodes.iter().any(|node| matches!(&node.body, Some(NodeBody::PaneId(pane)) if pane == &location.pane_id)))
}

fn terminal_matches(entries: &Entries, id: &str, location: &ViewSelection) -> bool {
    matches!(lookup(entries, EntityKind::Terminal, id, None),
        Some(Body::Terminal(terminal)) if terminal.location.as_ref() == Some(location))
}

fn lookup<'a>(
    entries: &'a Entries,
    kind: EntityKind,
    id: &str,
    agent_run_id: Option<&str>,
) -> Option<&'a Body> {
    let key = EntityKey {
        kind: kind as i32,
        id: id.to_owned(),
        agent_run_id: agent_run_id.map(str::to_owned),
    };
    entries.get(&key)?.body.as_ref()
}

fn evidence(evidence: Option<&EvidenceState>) -> bool {
    evidence.is_some_and(|value| {
        value.source != 0
            && value.confidence != 0
            && valid_display(&value.source_version)
            && value.uncertainty.as_deref().is_none_or(valid_display)
    })
}

fn valid_location(location: &ViewSelection) -> bool {
    valid_id(&location.workspace_id) && valid_id(&location.tab_id) && valid_id(&location.pane_id)
}

fn valid_display(value: &str) -> bool {
    value.len() <= 1024 && !value.chars().any(char::is_control)
}

fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(resync(message))
    }
}
