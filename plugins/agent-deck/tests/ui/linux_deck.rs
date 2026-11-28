#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use async_process::{Child, Command};
use corvid_agent_deck::connection::Client as DeckClient;
use corvid_agent_deck::desktop::FocusRequest;
use corvid_agent_deck_core::layout::{BAR_HEIGHT, GAP, KEY_HEIGHT, KEY_WIDTH};
use corvid_async::io::{AsyncRead, AsyncWrite};
use corvid_async::task::JoinSet;
use corvid_async::time::{sleep, timeout};
use corvid_async::{mpsc, watch};
use corvid_companion::{
    Config, HostEndpoint, PRESENTATION_ATTENTION_CAPABILITY, PRESENTATION_CAPABILITY,
    PRESENTATION_RUNS_CAPABILITY, PresentationAddress,
};
use corvid_presentation::desktop_focus::FocusFailure;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AgentBadge, AgentCollection, AgentProvider, CompanionConnection, DesktopAgentFocused,
    EvidenceConfidence, PresentationAttention, PresentationSlots, PresentedAttention, PresentedRun,
    PresentedRunKey, PresentedSlot, PromptObservation, RunAvailability,
};
use corvid_protocol::{
    COMMAND_TIMEOUT, FrameReader, FrameRole, FrameWriter, PeerRole, ProtocolFault, accept_hello,
};
use image::GenericImageView;
use uuid::Uuid;

fn deck_binary() -> PathBuf {
    std::env::var_os("CORVID_UI_DECK_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_corvid-agent-deck")))
}

fn fixture_run(
    key: PresentedRunKey,
    provider: AgentProvider,
    badge: AgentBadge,
    title: &str,
) -> PresentedRun {
    PresentedRun {
        key: Some(key),
        availability: RunAvailability::Available as i32,
        last_badge: badge as i32,
        provider: provider as i32,
        confidence: EvidenceConfidence::Confirmed as i32,
        collection: AgentCollection::Healthy as i32,
        session_title: Some(title.into()),
        ..Default::default()
    }
}

async fn serve_runs(
    address: <PresentationAddress as ToOwned>::Owned,
    controller: Uuid,
    runs: Vec<PresentedRun>,
    feed: watch::Receiver<Vec<PresentedAttention>>,
    synced: watch::Sender<bool>,
) -> Result<()> {
    let directory = corvid_protocol::unix::RuntimeDir::create_all(&address)?;
    let instance = directory.lock_instance()?;
    let listener = instance
        .bind(corvid_protocol::unix::SocketRole::Presentation)
        .await?;
    let mut peers = JoinSet::new();
    loop {
        let Some(stream) = listener.accept().await? else {
            continue;
        };
        let runs = runs.clone();
        let feed = feed.clone();
        let synced = synced.clone();
        peers.spawn(async move {
            let result = peer(
                corvid_async::io::adapt(stream),
                controller,
                runs,
                feed,
                synced,
            )
            .await;
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
    runs: Vec<PresentedRun>,
    mut feed: watch::Receiver<Vec<PresentedAttention>>,
    synced: watch::Sender<bool>,
) -> Result<()> {
    let first_key = runs
        .first()
        .and_then(|run| run.key.as_ref())
        .context("fixture run identity")?;
    let (input, output) = corvid_async::io::split(stream);
    let mut writer = FrameWriter::start(output, FrameRole::StructuredOnly).await?;
    let mut reader = FrameReader::start(input, FrameRole::StructuredOnly).await?;
    let hello = reader.next_envelope().await?.context("hello")?;
    let (session, reply) = accept_hello(
        &hello,
        PeerRole::Presentation,
        &first_key.host_id,
        &first_key.remote_epoch,
        &[
            PRESENTATION_CAPABILITY,
            PRESENTATION_RUNS_CAPABILITY,
            corvid_companion::PRESENTATION_WATCH_CAPABILITY,
            PRESENTATION_ATTENTION_CAPABILITY,
        ],
    )?;
    writer.envelope(&reply).await?;
    let mut revision = 1u64;
    while let Some(request) = reader.next_envelope().await? {
        let mut sent_runs = false;
        let body = match request.body {
            Some(Body::GetPresentationAttention(_)) => {
                Body::PresentationAttention(PresentationAttention {
                    controller_id: controller.to_string(),
                    connection: CompanionConnection::Ready as i32,
                    control_generation: Some(controller.to_string()),
                    remote_epoch: Some(first_key.remote_epoch.clone()),
                    attention: feed.borrow().clone(),
                    ..Default::default()
                })
            }
            Some(Body::GetPresentationRuns(_)) => {
                sent_runs = true;
                let slots = runs
                    .iter()
                    .enumerate()
                    .map(|(index, run)| PresentedSlot {
                        slot_id: index as u32 + 1,
                        revision: 1,
                        run: Some(run.clone()),
                        ..Default::default()
                    })
                    .collect();
                Body::PresentationRuns(corvid_protocol::wire::PresentationRuns {
                    layout_revision: 1,
                    state_revision: revision,
                    page: Some(PresentationSlots {
                        controller_id: controller.to_string(),
                        connection: CompanionConnection::Ready as i32,
                        control_generation: Some(controller.to_string()),
                        remote_epoch: Some(first_key.remote_epoch.clone()),
                        remote_sequence: Some(1),
                        slots,
                        ..Default::default()
                    }),
                })
            }
            Some(Body::WaitPresentationChange(wait)) => {
                if revision <= wait.after_revision {
                    if feed.changed().await.is_err() {
                        break;
                    }
                    let _ = feed.borrow_and_update();
                    revision += 1;
                }
                Body::PresentationChanged(corvid_protocol::wire::PresentationChanged { revision })
            }
            _ => anyhow::bail!("unexpected request"),
        };
        writer
            .envelope(&session.envelope(request.request_id, body))
            .await?;
        if sent_runs {
            let _ = synced.send(true);
        }
    }
    Ok(())
}

struct FixtureHost {
    controller: Uuid,
    run: PresentedRunKey,
    requests: mpsc::Receiver<FocusRequest>,
    _feed_tx: watch::Sender<Vec<PresentedAttention>>,
    _synced_rx: watch::Receiver<bool>,
}

impl FixtureHost {
    fn start(
        root: &Path,
        id: Uuid,
        endpoint: HostEndpoint,
        run_id: Uuid,
        jobs: &mut JoinSet<Result<()>>,
    ) -> Result<Self> {
        let state = match endpoint {
            HostEndpoint::Local { .. } => root.to_owned(),
            _ => root.join("slot-board").join(id.simple().to_string()),
        };
        corvid_companion::initialize(&state, id, endpoint, 8)?;
        let config = Config::read_if_present(&state)?.context("fixture config")?;
        let controller = config.controller_id;
        let address = root.join(&controller.simple().to_string()[..6]);
        let scope = corvid_companion::monitor::Scope {
            state,
            config,
            address: address.clone(),
        };
        scope.configure(&corvid_client::ControlEndpoint::Local(root.join("unused")))?;
        let epoch = Uuid::new_v4();
        let run = PresentedRunKey {
            host_id: id.to_string(),
            remote_epoch: epoch.to_string(),
            agent_run_id: run_id.to_string(),
        };
        let (feed_tx, feed) = watch::channel(Vec::new());
        let (synced_tx, synced_rx) = watch::channel(false);
        let mut presented = match id.as_u128() {
            1 => fixture_run(
                run.clone(),
                AgentProvider::Codex,
                AgentBadge::Working,
                "Model request",
            ),
            2 => fixture_run(
                run.clone(),
                AgentProvider::Claude,
                AgentBadge::Unknown,
                "Tool execution",
            ),
            _ => fixture_run(
                run.clone(),
                AgentProvider::Claude,
                AgentBadge::ResponseObserved,
                "Response observed",
            ),
        };
        if id.as_u128() == 1 {
            presented.prompt_observation = PromptObservation::ModelRequested as i32;
        } else if id.as_u128() == 2 {
            presented.unverified_requests = 1;
            presented.prompt_observation = PromptObservation::ToolExecuted as i32;
        }
        jobs.spawn(serve_runs(
            address,
            controller,
            vec![presented],
            feed,
            synced_tx,
        ));
        let (focus, requests) = mpsc::channel(1);
        jobs.spawn(corvid_presentation::desktop_focus::serve(
            id, controller, epoch, focus,
        ));
        Ok(Self {
            controller,
            run,
            requests,
            _feed_tx: feed_tx,
            _synced_rx: synced_rx,
        })
    }
}

#[compio::test]
async fn grid_routes_clicks_to_local_and_two_ssh_scopes_including_a_late_host() -> Result<()> {
    let root = tempfile::tempdir_in(std::env::home_dir().context("home")?)?;
    let state = root.path().join("companion");
    let mut jobs = JoinSet::new();
    // Sharing a provider run ID deliberately rules out lookup by a label or an
    // unqualified run. Metadata and click delivery use real local IPC sockets.
    let common_run = Uuid::new_v4();
    let mut local = FixtureHost::start(
        &state,
        Uuid::from_u128(1),
        HostEndpoint::from(&corvid_client::ControlEndpoint::Local(
            root.path().join("daemon"),
        )),
        common_run,
        &mut jobs,
    )?;
    let mut first = FixtureHost::start(
        &state,
        Uuid::from_u128(2),
        HostEndpoint::Ssh {
            alias: "first".into(),
        },
        common_run,
        &mut jobs,
    )?;
    let _manager = Command::new("openbox")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let built = deck_binary();
    let mut deck = Command::new(&built)
        .arg("--state-dir")
        .arg(&state)
        .kill_on_drop(true)
        .spawn()?;
    let window = window_id(&mut deck).await?;
    let ssh_scope = state
        .join("slot-board")
        .join(Uuid::from_u128(2).simple().to_string());
    let reopened = timeout(
        COMMAND_TIMEOUT,
        Command::new(&built)
            .arg("--state-dir")
            .arg(ssh_scope)
            .kill_on_drop(true)
            .status(),
    )
    .await
    .context("opening Agents from the SSH host did not reuse the running Deck")??;
    ensure!(reopened.success(), "reopening the shared Deck failed");
    ensure!(
        deck.try_status()?.is_none(),
        "the original Deck exited during relaunch"
    );
    let visible = Command::new("xdotool")
        .args(["search", "--onlyvisible", "--name", "^Corvid Agent Deck$"])
        .output()
        .await?;
    assert_eq!(
        String::from_utf8(visible.stdout)?
            .lines()
            .collect::<Vec<_>>(),
        [window.as_str()]
    );

    // A Deck opened from Desktop still uses the shared endpoint for its own
    // host, even when a legacy registration is present.
    let mut legacy = DeckClient::connect(Uuid::from_u128(1), local.controller, true).await?;
    click_card(&window, 0, &mut local, &mut jobs, Ok(())).await?;
    click_card(
        &window,
        0,
        &mut local,
        &mut jobs,
        Err(ProtocolFault::new("WorkspaceBusy", "another Desktop owns this workspace").into()),
    )
    .await?;
    let fallback = timeout(COMMAND_TIMEOUT, legacy.reader.next_envelope())
        .await??
        .context("the Desktop that opened Deck did not receive the writer-conflict retry")?;
    let Some(Body::FocusDesktopAgent(focus)) = fallback.body else {
        anyhow::bail!("unexpected request on the older Deck connection");
    };
    assert_eq!(focus.run, Some(local.run.clone()));
    legacy
        .reply(
            fallback.request_id.context("missing retry ID")?,
            Body::DesktopAgentFocused(DesktopAgentFocused {}),
        )
        .await?;
    click_card(&window, 1, &mut first, &mut jobs, Ok(())).await?;

    let mut second = FixtureHost::start(
        &state,
        Uuid::from_u128(3),
        HostEndpoint::Ssh {
            alias: "late".into(),
        },
        common_run,
        &mut jobs,
    )?;
    click_card(&window, 2, &mut second, &mut jobs, Ok(())).await?;
    if let Some(path) = std::env::var_os("CORVID_MULTI_HOST_GRID_SCREENSHOT") {
        capture_focused(Path::new(&path)).await?;
    }
    deck.kill()?;
    println!("PASS: Local and two SSH cards sent the exact host, epoch, and agent ID");
    Ok(())
}

#[compio::test]
async fn card_titles_animate_only_when_overflowing_the_card() -> Result<()> {
    let root = tempfile::tempdir_in(std::env::home_dir().context("home")?)?;
    let state = root.path().join("companion");
    let mut jobs = JoinSet::new();
    let host_id = Uuid::from_u128(1);
    corvid_companion::initialize(
        &state,
        host_id,
        HostEndpoint::from(&corvid_client::ControlEndpoint::Local(
            root.path().join("daemon"),
        )),
        8,
    )?;
    let config = Config::read_if_present(&state)?.context("fixture config")?;
    let controller = config.controller_id;
    let address = root.path().join(&controller.simple().to_string()[..6]);
    let scope = corvid_companion::monitor::Scope {
        state: state.clone(),
        config,
        address: address.clone(),
    };
    scope.configure(&corvid_client::ControlEndpoint::Local(
        root.path().join("unused"),
    ))?;
    let epoch = Uuid::new_v4();
    let short_run = fixture_run(
        PresentedRunKey {
            host_id: host_id.to_string(),
            remote_epoch: epoch.to_string(),
            agent_run_id: Uuid::new_v4().to_string(),
        },
        AgentProvider::Claude,
        AgentBadge::Ready,
        "Short",
    );
    let long_run = fixture_run(
        PresentedRunKey {
            host_id: host_id.to_string(),
            remote_epoch: epoch.to_string(),
            agent_run_id: Uuid::new_v4().to_string(),
        },
        AgentProvider::Codex,
        AgentBadge::Working,
        "Investigate very long agent thread title marquee scrolling",
    );
    let (_feed_tx, feed) = watch::channel(Vec::new());
    let (synced_tx, mut synced_rx) = watch::channel(false);
    jobs.spawn(serve_runs(
        address,
        controller,
        vec![short_run, long_run],
        feed,
        synced_tx,
    ));

    let _manager = Command::new("openbox")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut deck = Command::new(deck_binary())
        .arg("--state-dir")
        .arg(&state)
        .kill_on_drop(true)
        .spawn()?;
    let _window = window_id(&mut deck).await?;
    timeout(COMMAND_TIMEOUT, async {
        while !*synced_rx.borrow_and_update() {
            ensure!(
                synced_rx.changed().await.is_ok(),
                "fixture stopped before initial sync"
            );
        }
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("waiting for initial Agent Deck run sync")??;

    // Capture inside the initial 1.2s pause, then inside the active scroll phase.
    sleep(Duration::from_millis(350)).await;
    let frame1_path = root.path().join("frame1.png");
    capture_focused(&frame1_path).await?;
    sleep(Duration::from_millis(1800)).await;
    let frame2_path = root.path().join("frame2.png");
    capture_focused(&frame2_path).await?;

    if let Some(preview_dir) = std::env::var_os("CORVID_DECK_MARQUEE_PREVIEW_DIR") {
        let dir = PathBuf::from(preview_dir);
        std::fs::create_dir_all(&dir)?;
        std::fs::copy(&frame1_path, dir.join("deck-marquee-frame1.png"))?;
        std::fs::copy(&frame2_path, dir.join("deck-marquee-frame2.png"))?;
    }

    let frame1 = image::open(&frame1_path)?;
    let frame2 = image::open(&frame2_path)?;
    ensure!(
        frame1.dimensions() == frame2.dimensions(),
        "focused window dimensions changed between marquee frames"
    );

    let (width, height) = frame1.dimensions();
    let short_diff = diff_pixels(&frame1, &frame2, card_title_band(0, width, height));
    let long_diff = diff_pixels(&frame1, &frame2, card_title_band(1, width, height));
    deck.kill()?;

    ensure!(
        short_diff == 0,
        "expected short non-overflowing card title to stay static, observed {short_diff} differing pixels"
    );
    ensure!(
        long_diff > 0,
        "expected overflowing card title to animate horizontally, observed 0 differing pixels"
    );
    println!("PASS: short card diff={short_diff}, long marquee card diff={long_diff}");
    Ok(())
}

fn card_title_band(index: u32, image_width: u32, image_height: u32) -> (u32, u32, u32, u32) {
    let card_w = KEY_WIDTH as u32;
    let card_h = KEY_HEIGHT as u32;
    let gap = GAP as u32;
    let x0 = 12 + index * (card_w + gap) + 4;
    let x1 = (x0 + card_w - 8).min(image_width);
    // Anchor from the bottom of the client area: footer + grid py(8) + row 1 + gap + row 0.
    let row0_bottom = image_height.saturating_sub(BAR_HEIGHT as u32 + 8 + card_h + gap);
    let y0 = row0_bottom.saturating_sub(34);
    let y1 = row0_bottom.saturating_sub(4).min(image_height);
    (x0, y0, x1, y1)
}

fn diff_pixels(
    a: &image::DynamicImage,
    b: &image::DynamicImage,
    (x0, y0, x1, y1): (u32, u32, u32, u32),
) -> usize {
    let mut changed = 0;
    for y in y0..y1 {
        for x in x0..x1 {
            if a.get_pixel(x, y) != b.get_pixel(x, y) {
                changed += 1;
            }
        }
    }
    changed
}

async fn capture_focused(path: &Path) -> Result<()> {
    let status = Command::new("scrot")
        .args(["--overwrite", "--focused"])
        .arg(path)
        .status()
        .await?;
    ensure!(status.success(), "capturing focused Deck window");
    Ok(())
}

async fn window_id(deck: &mut Child) -> Result<String> {
    timeout(Duration::from_secs(10), async {
        loop {
            ensure!(deck.try_status()?.is_none(), "Deck stopped during startup");
            let shown = Command::new("xdotool")
                .args(["search", "--onlyvisible", "--name", "^Corvid Agent Deck$"])
                .output()
                .await?;
            if shown.status.success()
                && let Some(id) = String::from_utf8(shown.stdout)?.lines().next()
            {
                let status = Command::new("xdotool")
                    .args(["windowactivate", "--sync", id])
                    .status()
                    .await?;
                if status.success() {
                    return Ok(id.to_owned());
                }
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("opening the Agent Deck window")?
}

async fn click_card(
    window: &str,
    index: u32,
    expected: &mut FixtureHost,
    jobs: &mut JoinSet<Result<()>>,
    result: Result<(), FocusFailure>,
) -> Result<()> {
    let x = (55 + index * 94).to_string();
    timeout(Duration::from_secs(12), async {
        loop {
            let status = Command::new("xdotool")
                .args(["mousemove", "--window", window, &x, "85", "click", "1"])
                .status()
                .await?;
            ensure!(status.success(), "clicking the Deck card");
            corvid_async::select! {
                request = expected.requests.recv() => {
                    let request = request.context("Desktop focus request stream closed")?;
                    assert_eq!(request.run, expected.run);
                    request.result.send(result).map_err(|_| anyhow::anyhow!("Deck stopped waiting"))?;
                    return Ok(());
                }
                result = jobs.join_next() => anyhow::bail!("fixture server stopped: {result:?}"),
                _ = sleep(Duration::from_millis(350)) => {}
            }
        }
    })
    .await
    .with_context(|| format!("routing card {index} to {}", expected.run.host_id))?
}
