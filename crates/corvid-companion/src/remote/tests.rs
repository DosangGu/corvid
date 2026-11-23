use super::*;
use corvid_protocol::snapshot::SnapshotAssembler;
use corvid_protocol::wire::envelope::Body as Message;
use corvid_protocol::wire::*;
use corvid_protocol::{PeerRole, accept_hello, hello};

fn snapshot(entities: Vec<SnapshotEntity>) -> SynchronizedSnapshot {
    let greeting = hello("hello".into(), Some(Uuid::new_v4().to_string()), vec![]);
    let (session, _) = accept_hello(
        &greeting,
        PeerRole::Control,
        &Uuid::new_v4().to_string(),
        &Uuid::new_v4().to_string(),
        &[],
    )
    .unwrap();
    let chunks: Vec<_> = entities.chunks(128).collect();
    let mut assembler = SnapshotAssembler::new(session.clone(), "snapshot".into(), false);
    let mut messages = vec![Message::SnapshotBegin(SnapshotBegin {
        chunks: chunks.len() as u32,
        capabilities: vec![],
    })];
    messages.extend(chunks.into_iter().enumerate().map(|(index, chunk)| {
        Message::SnapshotChunk(SnapshotChunk {
            index: index as u32,
            entities: chunk.to_vec(),
            ..Default::default()
        })
    }));
    messages.push(Message::SnapshotEnd(SnapshotEnd {}));
    let mut result = None;
    for body in messages {
        let mut message = session.envelope(Some("snapshot".into()), body);
        message.sequence = Some(1);
        result = assembler.push(message).unwrap();
    }
    result.unwrap()
}

fn evidence() -> EvidenceState {
    EvidenceState {
        source: EvidenceSource::ProviderHook as i32,
        source_version: "fixture".into(),
        confidence: EvidenceConfidence::Confirmed as i32,
        ..Default::default()
    }
}

fn agent(run: Uuid, location: &ViewSelection, sequence: u64) -> AgentState {
    AgentState {
        summary: Some(AgentInfo {
            agent_run_id: run.to_string(),
            terminal_id: Uuid::new_v4().to_string(),
            location: Some(location.clone()),
            provider: AgentProvider::Claude as i32,
            process: AgentProcessState::Running as i32,
            badge: AgentBadge::Ended as i32,
            collection: AgentCollection::Healthy as i32,
            ..Default::default()
        }),
        lifecycle: AgentLifecycle::Ended as i32,
        activity: AgentActivity::Unknown as i32,
        lifecycle_evidence: Some(evidence()),
        process_evidence: Some(evidence()),
        activity_evidence: Some(evidence()),
        collection_evidence: Some(evidence()),
        created_sequence: Some(sequence),
        ..Default::default()
    }
}

fn workspace(location: &ViewSelection) -> Body {
    Body::Workspace(WorkspaceState {
        workspace_id: location.workspace_id.clone(),
        label: "Fixture".into(),
        tabs: vec![TabState {
            manual_name: None,
            tab_id: location.tab_id.clone(),
            label: "Fixture".into(),
            nodes: vec![LayoutNodeState {
                title: String::new(),
                manual_name: None,
                body: Some(layout_node_state::Body::PaneId(location.pane_id.clone())),
            }],
        }],
    })
}

#[test]
fn run_titles_follow_the_owning_pane_or_its_custom_single_pane_tab_name() {
    let location = ViewSelection {
        workspace_id: Uuid::new_v4().to_string(),
        tab_id: Uuid::new_v4().to_string(),
        pane_id: Uuid::new_v4().to_string(),
    };
    let Body::Workspace(mut layout) = workspace(&location) else {
        unreachable!();
    };
    layout.tabs[0].manual_name = Some("Release checklist".into());
    layout.tabs[0].nodes[0].title = "Shell".into();

    let split_tab = Uuid::new_v4().to_string();
    let [first, second] = std::array::from_fn(|_| ViewSelection {
        tab_id: split_tab.clone(),
        pane_id: Uuid::new_v4().to_string(),
        ..location.clone()
    });
    layout.tabs.push(TabState {
        tab_id: split_tab,
        label: "Reviews".into(),
        manual_name: Some("Reviews".into()),
        nodes: vec![
            LayoutNodeState {
                body: Some(layout_node_state::Body::Split(SplitState {
                    axis: SplitAxis::Horizontal as i32,
                    children: vec![1, 2],
                })),
                ..Default::default()
            },
            LayoutNodeState {
                body: Some(layout_node_state::Body::PaneId(first.pane_id.clone())),
                title: "Review SSH reconnect".into(),
                ..Default::default()
            },
            LayoutNodeState {
                body: Some(layout_node_state::Body::PaneId(second.pane_id.clone())),
                ..Default::default()
            },
        ],
    });

    let cases = [
        (Uuid::new_v4(), &location, Some("Release checklist")),
        (Uuid::new_v4(), &first, Some("Review SSH reconnect")),
        (Uuid::new_v4(), &second, None),
    ];
    let bodies = std::iter::once(Body::Workspace(layout)).chain(
        cases
            .iter()
            .map(|(run, location, _)| Body::Agent(Box::new(agent(*run, location, 1)))),
    );
    let cache = snapshot(
        bodies
            .map(|body| SnapshotEntity { body: Some(body) })
            .collect(),
    );
    let remote = Remote::read(&cache, &Uuid::new_v4().to_string()).unwrap();

    for (id, _, title) in cases {
        let (_, run) = remote.runs.iter().find(|(key, _)| key.run == id).unwrap();
        assert!(run.observation.eligible);
        assert_eq!(run.display.session_title.as_deref(), title);
    }
}

#[test]
fn excludes_confirmed_process_exits_from_admission_with_an_open_pane() {
    let location = ViewSelection {
        workspace_id: Uuid::new_v4().to_string(),
        tab_id: Uuid::new_v4().to_string(),
        pane_id: Uuid::new_v4().to_string(),
    };
    let bodies = [
        AgentProcessState::Running,
        AgentProcessState::Unknown,
        AgentProcessState::Exited,
    ]
    .into_iter()
    .enumerate()
    .map(|(index, process)| {
        let mut agent = agent(Uuid::new_v4(), &location, index as u64 + 1);
        agent.summary.as_mut().unwrap().process = process as i32;
        Body::Agent(Box::new(agent))
    });
    let cache = snapshot(
        std::iter::once(workspace(&location))
            .chain(bodies)
            .map(|body| SnapshotEntity { body: Some(body) })
            .collect(),
    );
    let remote = Remote::read(&cache, &Uuid::new_v4().to_string()).unwrap();
    let mut eligible = Vec::new();
    for run in remote.runs.values() {
        assert!(run.observation.ended);
        assert!(!run.observation.navigable);
        assert_eq!(run.observation.eligible, !run.process_exited);
        if run.observation.eligible {
            eligible.push(run.observation.created_sequence.unwrap());
        }
    }
    eligible.sort_unstable();
    assert_eq!(eligible, [1, 2]);
}

#[test]
fn bounded_attention_includes_queued_runs_and_request_metadata() {
    let first = Uuid::new_v4();
    let queued = Uuid::new_v4();
    let location = ViewSelection {
        workspace_id: Uuid::new_v4().to_string(),
        tab_id: Uuid::new_v4().to_string(),
        pane_id: Uuid::new_v4().to_string(),
    };
    let request = Uuid::new_v4().to_string();
    let mut bodies = vec![
        workspace(&location),
        Body::Agent(Box::new(agent(first, &location, 1))),
        Body::Agent(Box::new(agent(queued, &location, 2))),
        Body::Request(RequestState {
            agent_run_id: queued.to_string(),
            request_id: request.clone(),
            kind: InputRequestKind::Permission as i32,
            status: InputRequestStatus::Unverified as i32,
            evidence: Some(evidence()),
        }),
    ];
    for index in 0..=crate::PRESENTATION_ATTENTION_LIMIT {
        bodies.push(Body::Attention(AttentionState {
            agent_run_id: queued.to_string(),
            attention_id: Uuid::new_v4().to_string(),
            reason: AttentionReason::Input as i32,
            created_unix_ms: index as u64 + 1,
            request_id: Some(request.clone()),
            ..Default::default()
        }));
    }
    bodies.push(Body::Attention(AttentionState {
        agent_run_id: first.to_string(),
        attention_id: Uuid::new_v4().to_string(),
        reason: AttentionReason::Response as i32,
        acknowledged_unix_ms: Some(1),
        ..Default::default()
    }));
    let cache = snapshot(
        bodies
            .into_iter()
            .map(|body| SnapshotEntity { body: Some(body) })
            .collect(),
    );
    let remote = Remote::read(&cache, &Uuid::new_v4().to_string()).unwrap();
    let observations = remote
        .runs
        .iter()
        .map(|(key, run)| (*key, run.observation))
        .collect();
    let mut slots = crate::SlotLayout::new(1).unwrap();
    slots.reconcile(&observations, &BTreeSet::new()).unwrap();
    assert_eq!(slots.slots()[0].run.unwrap().run, first);
    assert_eq!(slots.overflow().next().unwrap().run, queued);
    assert_eq!(remote.attention.len(), crate::PRESENTATION_ATTENTION_LIMIT);
    assert_eq!(remote.omitted_attention, 1);
    assert!(
        remote
            .attention
            .iter()
            .all(
                |alert| alert.run.as_ref().unwrap().agent_run_id == queued.to_string()
                    && alert.request_kind == Some(InputRequestKind::Permission as i32)
                    && alert.request_status == Some(InputRequestStatus::Unverified as i32)
            )
    );
    assert!(
        remote
            .attention
            .windows(2)
            .all(|pair| pair[0].created_unix_ms > pair[1].created_unix_ms)
    );
    assert_eq!(
        remote.panes,
        BTreeSet::from([Uuid::parse_str(&location.pane_id).unwrap()])
    );
}

#[test]
fn priority_uses_request_opening_time_and_only_the_latest_unread_result() {
    let [waiting, finished, working] = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    let location = ViewSelection {
        workspace_id: Uuid::new_v4().to_string(),
        tab_id: Uuid::new_v4().to_string(),
        pane_id: Uuid::new_v4().to_string(),
    };
    let request = Uuid::new_v4().to_string();
    let mut bodies = vec![workspace(&location)];
    for (run, badge) in [
        (waiting, AgentBadge::NeedsInput),
        (finished, AgentBadge::ResponseObserved),
        (working, AgentBadge::Unknown),
    ] {
        let mut state = agent(run, &location, 1);
        let summary = state.summary.as_mut().unwrap();
        summary.badge = badge as i32;
        if run == working {
            state.prompt_observation = PromptObservation::ModelRequested as i32;
            summary.prompt_observed_unix_ms = Some(60);
            summary.unverified_requests = 1;
        }
        bodies.push(Body::Agent(Box::new(state)));
    }
    bodies.push(Body::Request(RequestState {
        agent_run_id: waiting.to_string(),
        request_id: request.clone(),
        kind: InputRequestKind::Question as i32,
        status: InputRequestStatus::Pending as i32,
        evidence: Some(EvidenceState {
            observed_unix_ms: 40,
            ..evidence()
        }),
    }));
    for (run, reason, created, acknowledged, request_id) in [
        (waiting, AttentionReason::Input, 10, Some(30), Some(request)),
        (finished, AttentionReason::Response, 20, None, None),
        (finished, AttentionReason::Response, 30, Some(40), None),
        (working, AttentionReason::Failure, 50, None, None),
    ] {
        bodies.push(Body::Attention(AttentionState {
            agent_run_id: run.to_string(),
            attention_id: Uuid::new_v4().to_string(),
            reason: reason as i32,
            created_unix_ms: created,
            acknowledged_unix_ms: acknowledged,
            request_id,
        }));
    }
    let cache = snapshot(
        bodies
            .into_iter()
            .map(|body| SnapshotEntity { body: Some(body) })
            .collect(),
    );
    let remote = Remote::read(&cache, &Uuid::new_v4().to_string()).unwrap();
    let display = |run| {
        &remote
            .runs
            .iter()
            .find(|(key, _)| key.run == run)
            .unwrap()
            .1
            .display
    };
    assert_eq!(display(waiting).pending_since_unix_ms, Some(10));
    assert_eq!(display(finished).new_result_unix_ms, None);
    assert_eq!(
        display(working).prompt_observation,
        PromptObservation::ModelRequested as i32
    );
    assert_eq!(display(working).prompt_observed_unix_ms, Some(60));
    assert_eq!(display(working).unverified_requests, 1);
    assert_eq!(display(working).new_result_unix_ms, None);
}
