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

#[cfg(unix)]
#[test]
fn reconnect_imports_the_current_observation_after_the_watch_is_ready() -> anyhow::Result<()> {
    corvid_async::task::worker("corvid-reconnect-test", |runtime| {
        runtime
            .block_on(check_observation_after_reconnect())
            .unwrap();
    })?
    .join()
    .expect("agent reconnect fixture panicked");
    Ok(())
}

#[cfg(unix)]
async fn check_observation_after_reconnect() -> anyhow::Result<()> {
    use std::time::Duration;

    use anyhow::Context;
    use corvid_async::oneshot;
    use corvid_async::time::{sleep, timeout};
    use corvid_client::ControlEndpoint;
    use corvid_protocol::unix::{RuntimeDir, SocketRole};

    use crate::{HostEndpoint, PresentationClient, initialize, run_native};

    let root = tempfile::tempdir_in(std::env::home_dir().context("home")?)?;
    let runtime = root.path().join("daemon");
    let state = root.path().join("state");
    let address = root.path().join("ipc");
    let directory = RuntimeDir::create_all(&runtime)?;
    let instance = directory.lock_instance()?;
    let listener = instance.bind_compio(SocketRole::Control).await?;
    let host = Uuid::new_v4();
    let epoch = Uuid::new_v4();
    let run = Uuid::new_v4();
    initialize(
        &state,
        host,
        HostEndpoint::from(&ControlEndpoint::Local(runtime)),
        8,
    )?;

    let location = ViewSelection {
        workspace_id: Uuid::new_v4().to_string(),
        tab_id: Uuid::new_v4().to_string(),
        pane_id: Uuid::new_v4().to_string(),
    };
    let (finish_sync, finish_requested) = oneshot::channel();
    let (disconnect, mut disconnect_requested) = oneshot::channel();
    let daemon = Box::pin(async {
        let mut finish_requested = Some(finish_requested);
        for (sequence, observation) in [
            (1, PromptObservation::Submitted),
            (2, PromptObservation::Responded),
            (3, PromptObservation::ToolExecuted),
        ] {
            let mut agent = agent(run, &location, 1);
            agent.lifecycle = AgentLifecycle::Live as i32;
            let summary = agent.summary.as_mut().unwrap();
            summary.badge = AgentBadge::Unknown as i32;
            summary.prompt_observed_unix_ms = Some(sequence * 100);
            agent.prompt_observation = observation as i32;
            let entities = [workspace(&location), Body::Agent(Box::new(agent))]
                .into_iter()
                .map(|body| SnapshotEntity { body: Some(body) })
                .collect();
            let stream = listener.accept().await?.context("missing control peer")?;
            let watch = serve_run_watch(
                stream,
                host,
                epoch,
                sequence,
                entities,
                (sequence == 2).then(|| finish_requested.take().unwrap()),
            );
            if sequence == 2 {
                corvid_async::select! {
                    _ = &mut disconnect_requested => {}
                    result = watch => result?,
                }
            } else {
                watch.await?;
            }
        }
        Ok::<_, anyhow::Error>(())
    });
    let consumer = Box::pin(async {
        let mut client = loop {
            if let Some(client) = PresentationClient::connect_if_present(&address).await? {
                break client;
            }
            sleep(Duration::from_millis(10)).await;
        };
        let first = wait_for_runs(&mut client, |page| {
            page.connection == CompanionConnection::Ready as i32 && page.remote_sequence == Some(1)
        })
        .await?;
        let key = first.slots[0].run.as_ref().unwrap().key.clone();
        assert_eq!(
            first.slots[0].run.as_ref().unwrap().prompt_observation,
            PromptObservation::Submitted as i32
        );
        let prior_generation = first.control_generation.context("initial generation")?;
        let focus = || {
            Message::FocusSlot(FocusSlot {
                control_generation: prior_generation.clone(),
                ..Default::default()
            })
        };
        client.reconnect_monitor().await?;
        let syncing = wait_for_runs(&mut client, |page| {
            page.connection == CompanionConnection::Syncing as i32
        })
        .await?;
        assert!(syncing.control_generation.is_none());
        assert!(syncing.slots.iter().all(|slot| slot.run.is_none()));
        let cached = client.slots(None).await?;
        let cached = cached.slots[0].run.as_ref().unwrap();
        assert_eq!(cached.key, key);
        assert_eq!(cached.availability, RunAvailability::Offline as i32);
        let Message::PresentationAttention(attention) = client
            .request(Message::GetPresentationAttention(
                GetPresentationAttention {},
            ))
            .await?
        else {
            anyhow::bail!("missing attention during synchronization");
        };
        assert!(attention.control_generation.is_none());
        assert!(attention.remote_epoch.is_none());
        let unavailable = client
            .request(focus())
            .await
            .expect_err("focus was accepted during synchronization");
        assert_eq!(
            unavailable.downcast_ref::<ProtocolFault>().unwrap().code,
            "ConnectionRequired"
        );
        finish_sync
            .send(())
            .expect("daemon did not wait for synchronization");

        let latest = wait_for_runs(&mut client, |page| {
            page.connection == CompanionConnection::Ready as i32 && page.remote_sequence == Some(2)
        })
        .await?;
        let current = latest.slots[0].run.as_ref().unwrap();
        assert_eq!(current.key, key);
        assert_eq!(
            current.prompt_observation,
            PromptObservation::Responded as i32
        );
        assert_eq!(current.prompt_observed_unix_ms, Some(200));
        assert_eq!(first.remote_epoch, latest.remote_epoch);
        assert_ne!(Some(prior_generation.clone()), latest.control_generation);
        let stale = client
            .request(focus())
            .await
            .expect_err("pre-reconnect focus was accepted");
        assert_eq!(
            stale.downcast_ref::<ProtocolFault>().unwrap().code,
            "StaleBinding"
        );

        disconnect
            .send(())
            .expect("daemon watch was already closed");
        let recovered = wait_for_runs(&mut client, |page| {
            page.connection == CompanionConnection::Ready as i32 && page.remote_sequence == Some(3)
        })
        .await?;
        let current = recovered.slots[0].run.as_ref().unwrap();
        assert_eq!(current.key, key);
        assert_eq!(
            current.prompt_observation,
            PromptObservation::ToolExecuted as i32
        );
        assert_eq!(current.prompt_observed_unix_ms, Some(300));
        assert_eq!(recovered.remote_epoch, latest.remote_epoch);
        assert_ne!(recovered.control_generation, latest.control_generation);
        Ok(())
    });
    let companion = Box::pin(run_native(&address, &state, std::future::pending()));
    timeout(Duration::from_secs(10), async {
        corvid_async::select! {
            result = consumer => result,
            result = daemon => result.and(Err(anyhow::anyhow!("test daemon stopped early"))),
            result = companion => {
                result.and(Err(anyhow::anyhow!("test companion stopped early")))
            }
        }
    })
    .await?
}

#[cfg(unix)]
async fn wait_for_runs(
    client: &mut crate::PresentationClient,
    matches: impl Fn(&PresentationSlots) -> bool,
) -> anyhow::Result<PresentationSlots> {
    loop {
        let reply = client
            .request(Message::GetPresentationRuns(GetPresentationRuns {
                page: 0,
            }))
            .await?;
        if let Message::PresentationRuns(PresentationRuns {
            page: Some(page), ..
        }) = reply
            && matches(&page)
        {
            return Ok(page);
        }
        corvid_async::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
async fn serve_run_watch(
    stream: compio::net::UnixStream,
    host: Uuid,
    epoch: Uuid,
    sequence: u64,
    entities: Vec<SnapshotEntity>,
    finish_sync: Option<corvid_async::oneshot::Receiver<()>>,
) -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use compio::io::util::Splittable;
    use corvid_protocol::snapshot::WATCH_CAPABILITY;
    use corvid_protocol::{CompioFrameReader, CompioFrameWriter, FrameRole};

    let (input, output) = stream.split();
    let role = FrameRole::StructuredOnly;
    let mut writer = CompioFrameWriter::start(output, role).await?;
    let mut reader = CompioFrameReader::start(input, role).await?;
    let hello = reader
        .next_envelope()
        .await?
        .context("missing control Hello")?;
    let (session, reply) = accept_hello(
        &hello,
        PeerRole::Control,
        &host.to_string(),
        &epoch.to_string(),
        &[WATCH_CAPABILITY],
    )?;
    writer.envelope(&reply).await?;
    let request = reader
        .next_envelope()
        .await?
        .context("missing watch request")?;
    ensure!(matches!(
        session.validate_request(&request)?,
        Message::SnapshotWatch(_)
    ));
    let subscription = Uuid::new_v4().to_string();
    let frame = |id, body| {
        let mut frame = session.envelope(id, body);
        frame.subscription_id = Some(subscription.clone());
        frame.sequence = Some(sequence);
        frame
    };
    for body in [
        Message::SnapshotBegin(SnapshotBegin {
            chunks: 1,
            capabilities: session.capabilities.clone(),
        }),
        Message::SnapshotChunk(SnapshotChunk {
            entities,
            ..Default::default()
        }),
        Message::SnapshotEnd(SnapshotEnd {}),
    ] {
        writer
            .envelope(&frame(request.request_id.clone(), body))
            .await?;
    }
    if let Some(finish_sync) = finish_sync {
        finish_sync.await?;
    }
    writer
        .envelope(&frame(None, Message::WatchReady(WatchReady {})))
        .await?;
    // Hold the first watch open until Companion abandons it for the explicit refresh.
    while let Some(ping) = reader.next_envelope().await? {
        ensure!(matches!(session.validate_request(&ping)?, Message::Ping(_)));
        writer
            .envelope(&session.envelope(ping.request_id, Message::Pong(Pong {})))
            .await?;
    }
    Ok(())
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
