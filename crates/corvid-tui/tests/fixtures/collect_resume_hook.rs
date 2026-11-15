//! The synthetic provider launches this child on Unix to verify collector acknowledgement.

use std::env;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use corvid_protocol::collector::{
    CAPABILITY, HOST_ID, LAUNCH_ASSOCIATION, RUNTIME_DIR, SERVER_EPOCH,
};
use corvid_protocol::unix::{RuntimeDir, SocketRole};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{AgentProvider, CollectHook, HookEvent};
use corvid_protocol::{
    FrameReader, FrameRole, FrameWriter, PeerRole, ProtocolFault, check_hello_result, hello,
};
use serde::Deserialize;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

#[derive(Deserialize)]
struct FixtureHook {
    hook_event_name: String,
    source: Option<String>,
    reason: Option<String>,
    session_id: String,
    prompt_id: Option<String>,
    tool_use_id: Option<String>,
}

#[tokio::test]
#[ignore = "started as an authenticated child of the synthetic provider"]
async fn collect() -> Result<()> {
    let provider = match env::var("CORVID_FIXTURE_PROVIDER")?.as_str() {
        "claude" => AgentProvider::Claude,
        "codex" => AgentProvider::Codex,
        _ => bail!("unknown fixture provider"),
    };
    let input: FixtureHook = serde_json::from_reader(io::stdin().lock())?;
    let event = match (
        input.hook_event_name.as_str(),
        input.source.as_deref(),
        input.reason.as_deref(),
    ) {
        ("SessionStart", Some("startup"), _) => HookEvent::Startup,
        ("SessionStart", Some("resume"), _) => HookEvent::Resume,
        ("SessionEnd", _, Some("resume")) => HookEvent::ResumeContextEnded,
        ("UserPromptSubmit", _, _) => HookEvent::PromptSubmitted,
        ("PreToolUse", _, _) => HookEvent::QuestionStarted,
        _ => bail!("unknown synthetic hook event"),
    };
    let launch_association = env::var(LAUNCH_ASSOCIATION)
        .context("synthetic provider launched without an observation association")?;
    let runtime = PathBuf::from(env::var_os(RUNTIME_DIR).context("missing fixture runtime")?);
    let directory = RuntimeDir::open(&runtime)?;
    let hook = CollectHook {
        launch_association,
        provider: provider as i32,
        event: event as i32,
        session_id: Some(input.session_id),
        turn_id: input.prompt_id,
        tool_id: input.tool_use_id,
        ..Default::default()
    };
    let host = env::var(HOST_ID)?;
    let epoch = env::var(SERVER_EPOCH)?;
    // A lost acknowledgement must not create a second delivery.
    let delivery = Uuid::new_v4().to_string();
    let attempt = || async {
        timeout(
            Duration::from_secs(1),
            submit(&directory, &host, &epoch, &delivery, &hook),
        )
        .await
        .context("synthetic hook submission timed out")?
    };
    for _ in 0..2 {
        match attempt().await {
            Ok(()) => return Ok(()),
            Err(error) if error.is::<ProtocolFault>() => return Err(error),
            Err(_) => sleep(Duration::from_millis(25)).await,
        }
    }
    attempt().await
}

async fn submit(
    directory: &RuntimeDir,
    host: &str,
    epoch: &str,
    delivery: &str,
    hook: &CollectHook,
) -> Result<()> {
    let stream = directory.connect(SocketRole::Collector).await?;
    let (read, write) = stream.into_split();
    let mut writer = FrameWriter::start(write, FrameRole::Collector).await?;
    let mut reader = FrameReader::start(read, FrameRole::Collector).await?;
    let greeting = hello(
        Uuid::new_v4().to_string(),
        None,
        vec![CAPABILITY.to_owned()],
    );
    writer.envelope(&greeting).await?;
    let reply = reader
        .next_envelope()
        .await?
        .context("collector closed during Hello")?;
    let session = check_hello_result(&greeting, &reply, PeerRole::Collector)?;
    ensure!(
        session.host_id == host && session.server_epoch == epoch,
        "fixture collector scope changed"
    );
    writer
        .envelope(&session.envelope(Some(delivery.to_owned()), Body::CollectHook(hook.clone())))
        .await?;
    let reply = reader
        .next_envelope()
        .await?
        .context("collector closed before acknowledging the hook")?;
    session.validate_scope(&reply)?;
    ensure!(
        reply.request_id.as_deref() == Some(delivery),
        "fixture collector replied to a different hook"
    );
    match reply.body {
        Some(Body::HookCollected(_)) => Ok(()),
        Some(Body::Error(fault)) => Err(ProtocolFault::from_wire(&fault).into()),
        _ => bail!("unexpected fixture collector response"),
    }
}
