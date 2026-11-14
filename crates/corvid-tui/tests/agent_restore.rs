#![cfg(any(target_os = "linux", windows))]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_client::{ControlEndpoint, InteractiveConnection};
use corvid_core::layout::{CwdObservation, CwdSource, Layout, Location};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AgentProvider, AgentRestoreMode, Attach, AttachRole, Attached, LaunchCommand,
    PaneRestoreStatus, RestoreWorkspace, Viewport,
};
use corvid_protocol::{Frame, PANE_WRITERS_CAPABILITY, RESTORE_AGENTS_CAPABILITY, decode_envelope};
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

#[cfg(target_os = "linux")]
use corvid_protocol::unix::RuntimeDir as StateDirectory;
#[cfg(windows)]
use corvid_protocol::windows::StateDirectory;

const BINARY: &str = env!("CARGO_BIN_EXE_corvid");
const WAIT: Duration = Duration::from_secs(20);

struct Fixture {
    _root: tempfile::TempDir,
    root: PathBuf,
    runtime: PathBuf,
    state: PathBuf,
    locations: Vec<Location>,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temporary = tempfile::tempdir_in(std::env::home_dir().context("home directory")?)?;
        let root = temporary.path().to_owned();
        let runtime = root.join("run");
        let state = root.join("state");
        let directory = StateDirectory::create_all(&state)?;
        let mut layout = Layout::empty_terminal();
        let first = layout.primary().unwrap();
        let mut locations = vec![first];
        for number in 1..7 {
            locations.push(layout.add_tab(first.workspace, format!("Fixture {number}"))?);
        }
        for location in &locations {
            layout.record_cwd(
                *location,
                CwdObservation {
                    path: root.as_os_str().as_encoded_bytes().to_vec(),
                    source: CwdSource::LaunchDirectory,
                    observed_unix_ms: 1,
                },
            )?;
        }
        directory.replace_private_file(
            "state.json",
            &serde_json::to_vec(&json!({
                "schema_version": 4, "layout": layout, "history": []
            }))?,
        )?;
        let output = root.join(if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        });
        let compiled = std::process::Command::new("rustc")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/resume_agent.rs"))
            .args(["--edition", "2024", "-o"])
            .arg(&output)
            .env(
                "CORVID_FIXTURE_CLAUDE",
                corvid_protocol::collector::CLAUDE_VERSION,
            )
            .env(
                "CORVID_FIXTURE_CODEX",
                corvid_protocol::collector::CODEX_VERSION,
            )
            .output()?;
        ensure!(
            compiled.status.success(),
            "fixture compilation failed: {}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        std::fs::copy(
            &output,
            root.join(if cfg!(windows) { "codex.exe" } else { "codex" }),
        )?;
        Ok(Self {
            _root: temporary,
            root,
            runtime,
            state,
            locations,
        })
    }

    async fn start(&self) -> Result<Child> {
        let mut paths = vec![self.root.clone()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let mut daemon = Command::new(BINARY)
            .arg("--runtime-dir")
            .arg(&self.runtime)
            .arg("daemon")
            .arg("--state-dir")
            .arg(&self.state)
            .env("PATH", std::env::join_paths(paths)?)
            .env("CORVID_FIXTURE_BINARY", BINARY)
            .kill_on_drop(true)
            .spawn()?;
        timeout(WAIT, async {
            while corvid_client::ping(&self.runtime).await.is_err() {
                ensure!(daemon.try_wait()?.is_none(), "fixture daemon exited");
                sleep(Duration::from_millis(25)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(daemon)
    }

    async fn connection(&self) -> Result<InteractiveConnection> {
        ControlEndpoint::Local(self.runtime.clone())
            .interactive(vec![RESTORE_AGENTS_CAPABILITY.into()])
            .await
    }

    async fn attach(&self, index: usize, launch: Option<LaunchCommand>) -> Result<Attached> {
        let location = self.locations[index];
        let mut connection = self.connection().await?;
        connection
            .send(Body::Attach(Attach {
                workspace_id: Some(location.workspace.to_string()),
                tab_id: Some(location.tab.to_string()),
                pane_id: Some(location.pane.to_string()),
                requested_role: AttachRole::Writer as i32,
                terminal_capabilities: vec!["legacy_vt.v1".into(), PANE_WRITERS_CAPABILITY.into()],
                viewport: Some(Viewport {
                    cols: 100,
                    rows: 30,
                }),
                launch,
                ..Default::default()
            }))
            .await?;
        let Frame::Structured(bytes) = timeout(WAIT, connection.receive()).await?? else {
            anyhow::bail!("expected attached response");
        };
        match decode_envelope(&bytes)?.body {
            Some(Body::Attached(attached)) => Ok(attached),
            other => anyhow::bail!("attach failed: {other:?}"),
        }
    }

    fn stored(&self) -> Result<Value> {
        let directory = StateDirectory::open(&self.state)?;
        Ok(serde_json::from_slice(
            &directory
                .read_private_file("state.json", 16 * 1024 * 1024)?
                .context("state file")?,
        )?)
    }

    fn request(&self, mode: AgentRestoreMode) -> RestoreWorkspace {
        RestoreWorkspace {
            workspace_id: self.locations[0].workspace.to_string(),
            pane_id: None,
            mode: mode as i32,
        }
    }
}

#[tokio::test]
async fn restores_exact_pane_conversations_after_loss_and_is_idempotent_across_clients()
-> Result<()> {
    let fixture = Fixture::new()?;
    let mut daemon = fixture.start().await?;
    let mut old_terminals = HashSet::new();
    for (index, provider, session) in [
        (0, AgentProvider::Claude, "first-a"),
        (1, AgentProvider::Claude, "session-b"),
        (2, AgentProvider::Codex, "session-c"),
    ] {
        let mut argv = vec![b"--fixture-session".to_vec(), session.as_bytes().to_vec()];
        if index == 0 {
            argv.extend([b"--fixture-switch".to_vec(), b"switched-a".to_vec()]);
        }
        let attached = fixture
            .attach(
                index,
                Some(LaunchCommand {
                    argv,
                    agent_provider: Some(provider as i32),
                    ..Default::default()
                }),
            )
            .await?;
        old_terminals.insert(attached.route_token.unwrap().terminal_id);
    }
    timeout(WAIT, async {
        loop {
            let stored = fixture.stored()?;
            if ["switched-a", "session-b", "session-c"].iter().enumerate().all(|(index, key)| {
                stored["resumes"][fixture.locations[index].pane.to_string()]["key"]["provider_session_id"] == *key
            }) { break Ok::<_, anyhow::Error>(()); }
            sleep(Duration::from_millis(50)).await;
        }
    }).await.context("waiting for verified saved conversations")??;
    let before = corvid_client::agents(&fixture.runtime).await?;
    ensure!(
        before.agents.iter().any(|agent| agent.pending_requests > 0),
        "fixture did not create historical input"
    );
    let old_runs: HashSet<_> = before
        .agents
        .iter()
        .map(|agent| agent.agent_run_id.clone())
        .collect();
    let old_epoch = fixture.connection().await?.session().server_epoch.clone();
    daemon.kill().await?;

    let mut stored = fixture.stored()?;
    ensure!(stored["schema_version"] == 5, "layout was not migrated");
    for (index, session) in [(4, "missing"), (5, "directory-missing"), (6, "mismatch")] {
        let mut saved = stored["resumes"][fixture.locations[0].pane.to_string()].clone();
        saved["key"]["provider_session_id"] = session.into();
        if index == 5 {
            saved["cwd"]["path"] =
                serde_json::to_value(fixture.root.join("absent").as_os_str().as_encoded_bytes())?;
        }
        stored["resumes"][fixture.locations[index].pane.to_string()] = saved;
    }
    StateDirectory::open(&fixture.state)?
        .replace_private_file("state.json", &serde_json::to_vec(&stored)?)?;
    let _restarted = fixture.start().await?;
    let mut first = fixture.connection().await?;
    let mut second = fixture.connection().await?;
    ensure!(
        first.session().server_epoch != old_epoch,
        "daemon epoch was reused"
    );
    let (one, two) = tokio::join!(
        first.restore_workspace(fixture.request(AgentRestoreMode::Resume)),
        second.restore_workspace(fixture.request(AgentRestoreMode::Resume)),
    );
    let one = one?;
    let two = two?;
    for result in [&one, &two] {
        ensure!(result.panes.len() == 7, "restore lost panes");
        for (index, pane) in result.panes.iter().enumerate() {
            match index {
                0..=2 => {
                    ensure!(
                        matches!(
                            pane.status(),
                            PaneRestoreStatus::Resumed | PaneRestoreStatus::Existing
                        ),
                        "resume failed: {pane:?}"
                    );
                    ensure!(
                        !old_terminals.contains(pane.terminal_id.as_ref().unwrap()),
                        "terminal identity reused"
                    );
                }
                3 => ensure!(
                    pane.status() == PaneRestoreStatus::Empty,
                    "shell-only pane launched"
                ),
                _ => ensure!(
                    pane.status() == PaneRestoreStatus::Failed,
                    "invalid session was resumed"
                ),
            }
        }
    }
    let current = corvid_client::agents(&fixture.runtime).await?;
    for (index, expected) in ["switched-a", "session-b", "session-c"]
        .into_iter()
        .enumerate()
    {
        let terminal = one.panes[index].terminal_id.as_ref().unwrap();
        let runs: Vec<_> = current
            .agents
            .iter()
            .filter(|agent| &agent.terminal_id == terminal && agent.terminal_available)
            .collect();
        ensure!(
            runs.len() == 1 && runs[0].provider_session_id.as_deref() == Some(expected),
            "wrong restored conversation"
        );
        ensure!(
            !old_runs.contains(&runs[0].agent_run_id) && runs[0].pending_requests == 0,
            "old run or question reactivated"
        );
    }
    let repeated = fixture
        .connection()
        .await?
        .restore_workspace(fixture.request(AgentRestoreMode::Resume))
        .await?;
    for index in 0..3 {
        ensure!(
            repeated.panes[index].terminal_id == one.panes[index].terminal_id,
            "repeat launched another terminal"
        );
    }
    // Native mismatch must not replace the saved exact session or leave a live terminal.
    let mut retry = fixture.request(AgentRestoreMode::Retry);
    retry.pane_id = Some(fixture.locations[6].pane.to_string());
    let retried = fixture.connection().await?.restore_workspace(retry).await?;
    ensure!(
        retried.panes[0].error.as_ref().unwrap().code == "ResumeSessionMismatch",
        "retry changed the saved target"
    );
    // All failures, including a missing directory, still allow explicit shell recovery.
    for index in 3..7 {
        fixture
            .attach(index, Some(LaunchCommand::default()))
            .await?;
    }
    Ok(())
}
