use super::*;
use corvid_protocol::wire::{
    AgentProvider, AttentionReason, CompanionConnection, PresentationSlots, PresentedAttention,
    PresentedRun, PresentedRunKey, PresentedSlot,
};
use corvid_protocol::{FrameReader, FrameRole, FrameWriter, PeerRole, accept_hello};
use tokio::io::{AsyncRead, AsyncWrite};

#[tokio::test]
async fn late_host_imports_existing_work_and_attention_then_continues_polling() -> Result<()> {
    #[cfg(not(windows))]
    let temporary = tempfile::tempdir_in(std::env::home_dir().context("home")?)?;
    #[cfg(windows)]
    let temporary = tempfile::tempdir()?;
    let state = temporary.path().join("companion");
    let current = Uuid::new_v4();
    let remote = Uuid::new_v4();
    let endpoint = corvid_companion::HostEndpoint::Ssh {
        alias: "fixture".into(),
    };
    corvid_companion::initialize(&state, current, endpoint.clone(), 8)?;
    #[cfg(windows)]
    let absent = Uuid::new_v4().simple().to_string();
    #[cfg(not(windows))]
    let absent = temporary.path().join("absent");
    corvid_companion::monitor::Scope {
        state: state.clone(),
        config: Config::read_if_present(&state)?.context("current config")?,
        address: absent.clone(),
    }
    .configure(&corvid_client::ControlEndpoint::Local(
        temporary.path().join("unused"),
    ))?;
    let (updates, mut received) = mpsc::channel(8);
    let mut jobs = tokio::task::JoinSet::new();
    jobs.spawn(poll_hosts(state.clone(), absent, current, updates));
    let catalog = crate::catalog::Catalog::start(state.clone(), current, UiSignal::default())?;
    timeout(Duration::from_secs(5), async {
        loop {
            if matches!(received.recv().await, Some(Update::Snapshot(id, Err(_))) if id == current)
            {
                break;
            }
        }
    })
    .await?;

    // The remote already has work and unread attention before this host is connected.
    let key = PresentedRunKey {
        host_id: remote.to_string(),
        remote_epoch: Uuid::new_v4().to_string(),
        agent_run_id: Uuid::new_v4().to_string(),
    };
    let alert = PresentedAttention {
        run: Some(key.clone()),
        attention_id: Uuid::new_v4().to_string(),
        created_unix_ms: now_ms(),
        reason: AttentionReason::Response as i32,
        provider: AgentProvider::Codex as i32,
        ..Default::default()
    };
    let (feed, receiver) = watch::channel(vec![alert]);
    let scoped = state.join("slot-board").join(remote.simple().to_string());
    corvid_companion::initialize(&scoped, remote, endpoint, 8)?;
    let config = Config::read_if_present(&scoped)?.context("fixture config")?;
    #[cfg(windows)]
    let address = config.controller_id.simple().to_string();
    #[cfg(not(windows))]
    let address = temporary.path().join("ipc");
    let scope = corvid_companion::monitor::Scope {
        state: scoped,
        config,
        address: address.clone(),
    };
    scope.configure(&corvid_client::ControlEndpoint::Local(
        temporary.path().join("unused"),
    ))?;
    let controller = scope.config.controller_id;
    let fixture_key = key.clone();
    jobs.spawn(async move {
        let result = serve(address, controller, fixture_key, receiver).await;
        if let Err(error) = &result {
            eprintln!("fixture server: {error:#}");
        }
        result
    });
    let mut ledger = Ledger::default();
    let first = next_snapshot(&mut received, remote).await?;
    assert_eq!(first.attention.len(), 1);
    assert_eq!(ledger.observe(remote, &first, now_ms())?.notices.len(), 1);
    timeout(Duration::from_secs(10), async {
        loop {
            let view = catalog.views.borrow().clone();
            if let Some(host) = view.hosts.get(&remote)
                && host.online
                && host.runs.first().and_then(|run| run.key.as_ref()) == Some(&key)
            {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await?;
    let again = next_snapshot(&mut received, remote).await?;
    assert!(ledger.observe(remote, &again, now_ms())?.notices.is_empty());
    feed.send_modify(|alerts| {
        let mut next = alerts[0].clone();
        next.attention_id = Uuid::new_v4().to_string();
        alerts.push(next);
    });
    timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = next_snapshot(&mut received, remote).await?;
            if snapshot.attention.len() == 2 {
                assert_eq!(
                    ledger.observe(remote, &snapshot, now_ms())?.notices.len(),
                    1
                );
                break Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    drop(catalog);
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    Ok(())
}

async fn next_snapshot(
    received: &mut mpsc::Receiver<Update>,
    host: Uuid,
) -> Result<PresentationAttention> {
    let mut issue = String::new();
    let result = timeout(Duration::from_secs(10), async {
        loop {
            if let Update::Snapshot(id, result) = received.recv().await.context("poller closed")?
                && id == host
            {
                match result {
                    Ok(snapshot) => return Ok(snapshot),
                    Err(error) => issue = error,
                }
            }
        }
    })
    .await;
    result.with_context(|| format!("remote notification feed timed out: {issue}"))?
}

async fn serve(
    address: <PresentationAddress as ToOwned>::Owned,
    controller: Uuid,
    key: PresentedRunKey,
    feed: watch::Receiver<Vec<PresentedAttention>>,
) -> Result<()> {
    #[cfg(not(windows))]
    let directory = corvid_protocol::unix::RuntimeDir::create_all(&address)?;
    #[cfg(not(windows))]
    let instance = directory.lock_instance()?;
    #[cfg(not(windows))]
    let listener = instance
        .bind(corvid_protocol::unix::SocketRole::Presentation)
        .await?;
    #[cfg(windows)]
    let mut listener = corvid_protocol::windows::PipeListener::bind(&address)?;
    let mut peers = tokio::task::JoinSet::new();
    loop {
        #[cfg(windows)]
        let stream = listener.accept().await?;
        #[cfg(not(windows))]
        let Some(stream) = listener.accept().await? else {
            continue;
        };
        let key = key.clone();
        let feed = feed.clone();
        peers.spawn(async move {
            let result = peer(stream, controller, key, feed).await;
            if let Err(error) = &result {
                eprintln!("fixture peer: {error:#}");
            }
            result
        });
    }
}

async fn peer(
    stream: impl AsyncRead + AsyncWrite + Unpin,
    controller: Uuid,
    key: PresentedRunKey,
    feed: watch::Receiver<Vec<PresentedAttention>>,
) -> Result<()> {
    let (input, output) = tokio::io::split(stream);
    let mut writer = FrameWriter::start(output, FrameRole::StructuredOnly).await?;
    let mut reader = FrameReader::start(input, FrameRole::StructuredOnly).await?;
    let hello = reader.next_envelope().await?.context("hello")?;
    let (session, reply) = accept_hello(
        &hello,
        PeerRole::Presentation,
        &key.host_id,
        &Uuid::new_v4().to_string(),
        &[
            corvid_companion::PRESENTATION_CAPABILITY,
            PRESENTATION_ATTENTION_CAPABILITY,
        ],
    )?;
    writer.envelope(&reply).await?;
    while let Some(request) = reader.next_envelope().await? {
        let body = match request.body {
            Some(Body::GetPresentationAttention(_)) => {
                Body::PresentationAttention(PresentationAttention {
                    controller_id: controller.to_string(),
                    connection: CompanionConnection::Ready as i32,
                    control_generation: Some(controller.to_string()),
                    remote_epoch: Some(key.remote_epoch.clone()),
                    attention: feed.borrow().clone(),
                    ..Default::default()
                })
            }
            Some(Body::GetPresentationSlots(_)) => Body::PresentationSlots(PresentationSlots {
                controller_id: controller.to_string(),
                connection: CompanionConnection::Ready as i32,
                control_generation: Some(controller.to_string()),
                remote_epoch: Some(key.remote_epoch.clone()),
                slots: vec![PresentedSlot {
                    run: Some(PresentedRun {
                        key: Some(key.clone()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            _ => anyhow::bail!("unexpected request"),
        };
        writer
            .envelope(&session.envelope(request.request_id, body))
            .await?;
    }
    Ok(())
}
