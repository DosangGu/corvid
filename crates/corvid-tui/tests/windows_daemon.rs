#![cfg(windows)]

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::num::NonZeroU16;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::process::{Child as Process, Command, Stdio};
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use corvid_protocol::windows::DaemonPipe;
use corvid_protocol::wire::snapshot_entity::Body;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
};

const CORVID: &str = env!("CARGO_BIN_EXE_corvid");
const WAIT: Duration = Duration::from_secs(12);
const ROWS: u16 = 30;
const COLS: u16 = 100;

#[tokio::test]
async fn tab_rename_survives_a_split_and_panes_follow_their_terminals() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let runtime = fixture.path().join("run");
    ensure_daemon(&runtime, &fixture.path().join("state")).await?;
    let _daemon = BackgroundDaemon::open(&runtime).await?;
    let mut tui = Tui::open(
        &runtime,
        [
            "attach",
            "--new",
            "--",
            "powershell.exe",
            "-NoLogo",
            "-NoProfile",
        ]
        .map(OsString::from),
    )?;
    tui.wait_for("PS ")?;
    tui.send(b"[Console]::Title='AUTO_ONE'\r")?;
    wait_for_title(&runtime, "AUTO_ONE", "AUTO_ONE").await?;

    tui.send(b"\x1d.")?;
    tui.wait_for("Rename tab")?;
    tui.send("이름 🐦".as_bytes())?;
    tui.send(b"\r")?;
    tui.wait_without("Rename tab")?;

    tui.send(b"[Console]::Title='AUTO_TWO'; [Console]::WriteLine(('TITLE_' + 'UPDATED'))\r")?;
    tui.wait_for("TITLE_UPDATED")?;
    wait_for_title(&runtime, "이름 🐦", "AUTO_TWO").await?;

    tui.send(b"\x1d%")?;
    tui.wait_without("TITLE_UPDATED")?;
    tui.send(b"[Console]::Title='SPLIT_AUTO'\r")?;
    wait_for_title(&runtime, "이름 🐦", "SPLIT_AUTO").await?;
    tui.send(b"\x1d.")?;
    tui.wait_for("Rename tab")?;
    tui.send(b"\x1b[200~Manual\r\nTab\x1b[201~")?;
    tui.wait_for("> ManualTab")?;
    tui.send(b"\r")?;
    tui.wait_without("Rename tab")?;
    wait_for_title(&runtime, "ManualTab", "SPLIT_AUTO").await?;
    wait_for_title(&runtime, "ManualTab", "AUTO_TWO").await?;

    tui.send(b"\x1d.")?;
    tui.wait_for("Rename tab")?;
    tui.send(b"\r")?;
    tui.wait_without("Rename tab")?;
    wait_for_title(&runtime, "Tab 1", "SPLIT_AUTO").await?;
    tui.wait_for("Tab 1")?;

    tui.send(b"\x1dX")?;
    tui.wait_without("SPLIT_AUTO")?;
    wait_for_title(&runtime, "AUTO_TWO", "AUTO_TWO").await?;
    tui.send(b"exit\r")?;
    tui.expect_exit(0)?;
    Ok(())
}

async fn wait_for_title(runtime: &Path, tab_name: &str, pane_name: &str) -> Result<()> {
    let mut observed = Vec::new();
    tokio::time::timeout(WAIT, async {
        loop {
            let snapshot = corvid_client::snapshot(runtime).await?;
            observed = snapshot.snapshot().entities().filter_map(|entity| {
                if let Some(Body::Workspace(workspace)) = &entity.body {
                    Some(workspace.tabs.iter().map(|tab| (tab.label.clone(), tab.nodes.iter().map(|node| node.title.clone()).collect::<Vec<_>>())).collect::<Vec<_>>())
                } else { None }
            }).collect::<Vec<_>>();
            if snapshot.snapshot().entities().any(|entity| {
                matches!(&entity.body, Some(Body::Workspace(workspace)) if workspace.tabs.iter().any(|tab| tab.label == tab_name && tab.nodes.iter().any(|node| node.title == pane_name)))
            }) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }).await.with_context(|| format!("waiting for tab {tab_name:?}, pane {pane_name:?}; observed {observed:?}"))?
}

#[tokio::test]
async fn command_menu_consumes_input_and_paste_without_resizing_the_terminal() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let runtime = fixture.path().join("run");
    ensure_daemon(&runtime, &fixture.path().join("state")).await?;
    let _daemon = BackgroundDaemon::open(&runtime).await?;
    let mut tui = Tui::open(
        &runtime,
        [
            "attach",
            "--new",
            "--",
            "powershell.exe",
            "-NoLogo",
            "-NoProfile",
        ]
        .map(OsString::from),
    )?;
    tui.wait_for("PS ")?;
    let original = terminal_id(&runtime).await?;
    tui.send(b"[Console]::WriteLine(('BEFORE_' + [Console]::WindowHeight + 'x' + [Console]::WindowWidth))\r")?;
    tui.wait_for("BEFORE_29x100")?;
    tui.send(b"\x1d?")?;
    tui.wait_for("CORVID COMMANDS")?;
    tui.send(b"QQQ")?;
    tui.send(b"\x1b[200~$env:CORVID_MENU_LEAK='yes'\r\x1b[201~")?;
    tokio::time::sleep(Duration::from_millis(2200)).await;
    tui.send(b"?")?;
    tui.wait_for("CORVID COMMANDS")?;
    tui.send(b"\x1b")?;
    tui.wait_without("CORVID COMMANDS")?;
    tui.send(b"if (!$env:CORVID_MENU_LEAK) { [Console]::WriteLine(('MENU_' + 'CLEAN')) }; [Console]::WriteLine(('AFTER_' + [Console]::WindowHeight + 'x' + [Console]::WindowWidth))\r")?;
    tui.wait_for("MENU_CLEAN")?;
    tui.wait_for("AFTER_29x100")?;
    assert_eq!(terminal_id(&runtime).await?, original);
    tui.send(b"exit\r")?;
    tui.expect_exit(0)?;
    Ok(())
}

#[tokio::test]
async fn ordinary_attach_resumes_the_terminal_and_updates_its_size() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let runtime = fixture.path().join("run");
    ensure_daemon(&runtime, &fixture.path().join("state")).await?;
    let _daemon = BackgroundDaemon::open(&runtime).await?;
    let mut writer = Tui::open(
        &runtime,
        [
            "attach",
            "--new",
            "--",
            "powershell.exe",
            "-NoLogo",
            "-NoProfile",
        ]
        .map(OsString::from),
    )?;
    writer.wait_for("PS ")?;
    writer.send(b"$kept='PRESERVED'; [Console]::WriteLine(('STATE_' + $kept))\r")?;
    writer.wait_for("STATE_PRESERVED")?;
    let id = terminal_id(&runtime).await?;

    let mut successor = Tui::open_sized(&runtime, ["attach"].map(OsString::from), 72, 22)?;
    writer.expect_exit(0)?;
    successor.wait_for("STATE_PRESERVED")?;
    assert_eq!(terminal_id(&runtime).await?, id);
    successor.send(b"'RESUMED_'+$kept+'_'+[Console]::WindowWidth\r")?;
    successor.wait_for("RESUMED_PRESERVED_72")?;
    successor.send(b"exit\r")?;
    successor.expect_exit(0)?;
    Ok(())
}

#[tokio::test]
async fn two_windows_clients_write_to_different_tabs_in_the_same_workspace() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let runtime = fixture.path().join("run");
    ensure_daemon(&runtime, &fixture.path().join("state")).await?;
    let _daemon = BackgroundDaemon::open(&runtime).await?;
    let shell = ["--", "powershell.exe", "-NoLogo", "-NoProfile"];
    let mut first = Tui::open(
        &runtime,
        ["attach", "--new"]
            .into_iter()
            .chain(shell)
            .map(OsString::from),
    )?;
    first.wait_for("PS ")?;
    let mut second = Tui::open(
        &runtime,
        ["attach", "--new-tab", "Second"]
            .into_iter()
            .chain(shell)
            .map(OsString::from),
    )?;
    second.wait_for("PS ")?;
    second.resize(72, 22)?;
    second.wait_for("CORVID")?;
    first.send(b"[Console]::WriteLine(('FIRST_' + [Console]::WindowWidth))\r")?;
    second.send(b"[Console]::WriteLine(('SECOND_' + [Console]::WindowWidth))\r")?;
    first.wait_for("FIRST_100")?;
    second.wait_for("SECOND_72")?;
    first.wait_without("SECOND_72")?;
    second.wait_without("FIRST_100")?;
    first.send(b"exit\r")?;
    second.send(b"exit\r")?;
    first.expect_exit(0)?;
    second.expect_exit(0)?;
    Ok(())
}

#[tokio::test]
async fn takeover_and_disconnect_keep_the_shells_and_other_workspaces_running() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let runtime = fixture.path().join("run");
    ensure_daemon(&runtime, &fixture.path().join("state")).await?;
    let _daemon = BackgroundDaemon::open(&runtime).await?;
    let shell = ["--", "powershell.exe", "-NoLogo", "-NoProfile"];
    let open = |options: &[&str]| {
        Tui::open(
            &runtime,
            options.iter().copied().chain(shell).map(OsString::from),
        )
    };
    let mut first = open(&["attach", "--new"])?;
    first.wait_for("PS ")?;
    first.send(b"$kept='SAME'; [Console]::WriteLine(('WRITER_' + $kept))\r")?;
    first.wait_for("WRITER_SAME")?;
    let snapshot = corvid_client::snapshot(&runtime).await?;
    let selection = snapshot
        .snapshot()
        .entities()
        .find_map(|entity| {
            if let Some(Body::Terminal(terminal)) = &entity.body {
                terminal.location.clone()
            } else {
                None
            }
        })
        .context("fixture terminal location")?;
    let terminal = terminal_id(&runtime).await?;

    let mut second = open(&["attach", "--new-tab", "Second"])?;
    second.wait_for("PS ")?;
    second.send(b"$peer='RETAINED'; [Console]::WriteLine(('PEER_' + $peer))\r")?;
    second.wait_for("PEER_RETAINED")?;
    let snapshot = corvid_client::snapshot(&runtime).await?;
    let peer = snapshot
        .snapshot()
        .entities()
        .find_map(|entity| {
            if let Some(Body::Terminal(terminal)) = &entity.body {
                terminal
                    .location
                    .clone()
                    .filter(|location| location.tab_id != selection.tab_id)
            } else {
                None
            }
        })
        .context("peer terminal location")?;
    let mut other = open(&["attach", "--new-workspace", "Other"])?;
    other.wait_for("PS ")?;
    let mut successor = Tui::open(
        &runtime,
        [
            "attach",
            "--takeover",
            "--workspace",
            &selection.workspace_id,
            "--tab",
            &selection.tab_id,
            "--pane",
            &selection.pane_id,
        ]
        .map(OsString::from),
    )?;
    first.expect_exit(0).context("previous writer detached")?;
    successor.wait_for("WRITER_SAME")?;
    successor.send(b"[Console]::WriteLine(('TAKEOVER_' + $kept))\r")?;
    successor.wait_for("TAKEOVER_SAME")?;
    let snapshot = corvid_client::snapshot(&runtime).await?;
    assert!(snapshot.snapshot().entities().any(|entity| {
        matches!(&entity.body, Some(Body::Terminal(state)) if state.terminal_id == terminal)
    }));

    successor.send(b"\x1dD")?;
    second.expect_exit(0).context("peer writer disconnected")?;
    other.send(b"[Console]::WriteLine(('OTHER_' + 'STILL_HERE'))\r")?;
    other.wait_for("OTHER_STILL_HERE")?;
    successor.send(b"[Console]::WriteLine(('STILL_' + $kept))\r")?;
    successor.wait_for("STILL_SAME")?;
    let mut rejoined = Tui::open(
        &runtime,
        [
            "attach",
            "--workspace",
            &peer.workspace_id,
            "--tab",
            &peer.tab_id,
            "--pane",
            &peer.pane_id,
        ]
        .map(OsString::from),
    )?;
    rejoined.wait_for("PEER_RETAINED")?;
    rejoined.send(b"[Console]::WriteLine(('REJOINED_' + $peer))\r")?;
    rejoined.wait_for("REJOINED_RETAINED")?;
    for client in [&mut successor, &mut rejoined, &mut other] {
        client.send(b"exit\r")?;
        client.expect_exit(0)?;
    }
    Ok(())
}

#[tokio::test]
async fn native_agent_launch_tracks_the_owned_child_through_exit() -> Result<()> {
    use corvid_protocol::wire::{
        AgentCollection, AgentLifecycle, AgentProcessState, AgentProvider, ProviderExecutionMode,
    };

    let fixture = tempfile::tempdir()?;
    let runtime = fixture.path().join("run");
    let programs = fixture.path().join("programs");
    std::fs::create_dir(&programs)?;
    let system_root = std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?;
    // The fixture has no provider hooks. Its exit is observed through ConPTY's owned child.
    std::fs::copy(
        std::path::PathBuf::from(system_root).join("System32/cmd.exe"),
        programs.join("codex.exe"),
    )?;
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(programs).chain(std::env::split_paths(&inherited_path)),
    )?;
    let started = Command::new(CORVID)
        .env("PATH", path)
        .arg("--runtime-dir")
        .arg(&runtime)
        .args(["ensure-daemon", "--state-dir"])
        .arg(fixture.path().join("state"))
        .output()?;
    ensure!(started.status.success(), "fixture daemon failed to start");
    let _daemon = BackgroundDaemon::open(&runtime).await?;
    let args = [
        "attach",
        "--new-tab",
        "Native",
        "--agent",
        "codex",
        "--",
        "/d",
        "/q",
        "/k",
        "echo NATIVE_AGENT",
    ]
    .map(OsString::from);
    let mut tui = Tui::open(&runtime, args)?;
    tui.wait_for("NATIVE_AGENT")?;

    let running = native_agent(&runtime, AgentProcessState::Running).await?;
    let run = running.summary.context("missing native agent summary")?;
    assert_eq!(run.provider, AgentProvider::Codex as i32);
    assert_eq!(run.collection, AgentCollection::NotRegistered as i32);
    assert_eq!(
        run.adapter.unwrap().mode,
        ProviderExecutionMode::ProcessOnly as i32
    );
    assert!(run.terminal_available);

    tui.send(b"exit 7\r")?;
    tui.expect_exit(7)?;
    let ended = native_agent(&runtime, AgentProcessState::Exited).await?;
    assert_eq!(ended.lifecycle, AgentLifecycle::Ended as i32);
    let ended = ended.summary.context("missing ended agent summary")?;
    assert_eq!(ended.agent_run_id, run.agent_run_id);
    assert_eq!(ended.location, run.location);
    assert_eq!(ended.exit_code, Some(7));
    assert!(!ended.terminal_available);
    Ok(())
}

#[tokio::test]
async fn detects_agents_started_inside_an_existing_windows_shell() -> Result<()> {
    use corvid_protocol::wire::{AgentCollection, AgentProcessState, AgentProvider};

    let fixture = tempfile::tempdir()?;
    let runtime = fixture.path().join("run");
    let system_root = std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?;
    let executable = fixture.path().join("claude.exe");
    std::fs::copy(
        std::path::PathBuf::from(system_root).join("System32/cmd.exe"),
        &executable,
    )?;
    ensure_daemon(&runtime, &fixture.path().join("state")).await?;
    let _daemon = BackgroundDaemon::open(&runtime).await?;
    let mut tui = Tui::open(
        &runtime,
        [
            "attach",
            "--new",
            "--",
            "powershell.exe",
            "-NoLogo",
            "-NoProfile",
        ]
        .map(OsString::from),
    )?;
    tui.wait_for("PS ")?;
    let terminal = terminal_id(&runtime).await?;
    let launch = format!(
        "& '{}' /d /q /k 'echo AUTO_'+'AGENT'\r",
        executable.display()
    );
    // The image is a stand-in for the CLI. The shell and subprocess are native ConPTY children.
    tui.send(launch.as_bytes())?;
    let running = native_agent(&runtime, AgentProcessState::Running).await?;
    let run = running.summary.context("missing detected agent")?;
    assert_eq!(run.terminal_id, terminal);
    assert_eq!(run.provider, AgentProvider::Claude as i32);
    assert_eq!(run.collection, AgentCollection::NotRegistered as i32);

    tui.send(b"exit 6\r")?;
    let ended = native_agent(&runtime, AgentProcessState::Exited).await?;
    let ended = ended.summary.context("missing ended agent")?;
    assert_eq!(ended.agent_run_id, run.agent_run_id);
    assert_eq!(ended.exit_code, Some(6));
    assert_eq!(terminal_id(&runtime).await?, terminal);

    tui.send(launch.as_bytes())?;
    let next = native_agent(&runtime, AgentProcessState::Running).await?;
    let next = next.summary.context("missing second agent")?;
    assert_ne!(next.agent_run_id, run.agent_run_id);
    assert_eq!(next.terminal_id, terminal);
    tui.send(b"exit\rexit\r")?;
    tui.expect_exit(0)?;
    Ok(())
}

#[test]
fn native_hook_provider_fixture() -> Result<()> {
    let Some(helper) = std::env::var_os("CORVID_TEST_HOOK_HELPER") else {
        return Ok(());
    };
    let provider = if std::env::current_exe()?
        .file_stem()
        .is_some_and(|stem| stem == "codex")
    {
        "codex"
    } else {
        "claude"
    };
    let config = Command::new(&helper).args(["hooks", provider]).output()?;
    ensure!(
        config.status.success(),
        "could not generate hook configuration"
    );
    let config: serde_json::Value = serde_json::from_slice(&config.stdout)?;
    for (event, event_fields) in [
        ("SessionStart", serde_json::json!({"source": "startup"})),
        (
            "UserPromptSubmit",
            serde_json::json!({"turn_id": "turn-1", "prompt_id": "turn-1", "prompt": "discarded fixture text"}),
        ),
        (
            "Stop",
            serde_json::json!({"turn_id": "turn-1", "prompt_id": "turn-1", "stop_hook_active": false}),
        ),
    ] {
        let handler = &config["hooks"][event][0]["hooks"][0];
        let invocation = handler["command"].as_str().context("missing command")?;
        let mut command = if provider == "codex" {
            let mut command = Command::new("powershell.exe");
            command.args(["-NoProfile", "-Command", invocation]);
            command
        } else {
            let mut command = Command::new(invocation);
            for arg in handler["args"].as_array().context("missing arguments")? {
                command.arg(arg.as_str().context("invalid argument")?);
            }
            command
        };
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut fields = event_fields;
        fields["hook_event_name"] = event.into();
        fields["session_id"] = format!("native-{provider}-fixture").into();
        child
            .stdin
            .take()
            .context("missing hook stdin")?
            .write_all(fields.to_string().as_bytes())?;
        let result = child.wait_with_output()?;
        assert!(result.status.success() && result.stderr.is_empty());
        if event == "Stop" && provider == "codex" {
            assert_eq!(String::from_utf8(result.stdout)?.trim(), "{}");
        } else {
            assert!(result.stdout.is_empty());
        }
    }
    println!("HOOK_FIXTURE_READY");
    std::io::stdin().read_line(&mut String::new())?;
    Ok(())
}

#[tokio::test]
async fn windows_shell_hooks_publish_only_bound_provider_observations() -> Result<()> {
    use corvid_protocol::wire::{
        AgentCollection, AgentProcessState, AttentionReason, CollectHook, HookEvent,
    };

    for provider in ["claude", "codex"] {
        let fixture = tempfile::tempdir()?;
        let runtime = fixture.path().join("run");
        ensure_daemon(&runtime, &fixture.path().join("state")).await?;
        let _daemon = BackgroundDaemon::open(&runtime).await?;
        let executable = fixture.path().join(format!("{provider}.exe"));
        std::fs::copy(std::env::current_exe()?, &executable)?;
        let mut tui = Tui::open(
            &runtime,
            [
                "attach",
                "--new",
                "--",
                "powershell.exe",
                "-NoLogo",
                "-NoProfile",
            ]
            .map(OsString::from),
        )?;
        tui.wait_for("PS ")?;
        let launch = format!(
            "$env:CORVID_TEST_HOOK_HELPER='{}'; & '{}' --exact native_hook_provider_fixture --nocapture\r",
            CORVID.replace('\'', "''"),
            executable.to_string_lossy().replace('\'', "''"),
        );
        tui.send(launch.as_bytes())?;
        tui.wait_for("HOOK_FIXTURE_READY")?;
        let agent = native_agent(&runtime, AgentProcessState::Running).await?;
        let run = agent.summary.context("missing native hook run")?;
        assert_eq!(
            run.provider_session_id,
            Some(format!("native-{provider}-fixture"))
        );
        assert_eq!(run.collection, AgentCollection::Healthy as i32);
        let host = corvid_client::host(&runtime).await?;
        let unbound = corvid_client::submit_hook(
            &runtime,
            &host.host_id,
            &host.server_epoch,
            CollectHook {
                launch_association: run.terminal_id.clone(),
                provider: run.provider,
                event: HookEvent::DeliveryGap as i32,
                ..Default::default()
            },
            tokio::time::Instant::now() + WAIT,
        )
        .await
        .unwrap_err();
        assert_eq!(
            unbound
                .downcast_ref::<corvid_protocol::ProtocolFault>()
                .map(|fault| fault.code.as_str()),
            Some("Unauthorized")
        );
        let snapshot = corvid_client::snapshot(&runtime).await?;
        assert!(snapshot.snapshot().entities().any(|entry| {
            matches!(&entry.body, Some(Body::Attention(attention)) if attention.agent_run_id == run.agent_run_id
                && attention.reason == AttentionReason::Response as i32)
        }));
        tui.send(b"\rexit\r")?;
        tui.expect_exit(0)?;
    }
    Ok(())
}

async fn native_agent(
    runtime: &Path,
    process: corvid_protocol::wire::AgentProcessState,
) -> Result<corvid_protocol::wire::AgentState> {
    tokio::time::timeout(WAIT, async {
        loop {
            let snapshot = corvid_client::snapshot(runtime).await?;
            if let Some(agent) =
                snapshot
                    .snapshot()
                    .entities()
                    .find_map(|entity| match &entity.body {
                        Some(Body::Agent(agent))
                            if agent
                                .summary
                                .as_ref()
                                .is_some_and(|summary| summary.process == process as i32) =>
                        {
                            Some(agent.as_ref().clone())
                        }
                        _ => None,
                    })
            {
                return Ok(agent);
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .context("native agent state was not published")?
}

#[tokio::test]
async fn upgrade_preserves_the_windows_shell_and_screen() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let runtime = fixture.path().join("run");
    let state = fixture.path().join("state");
    ensure_daemon(&runtime, &state).await?;
    let old_daemon = BackgroundDaemon::open(&runtime).await?;
    let original = corvid_client::host(&runtime).await?;
    let powershell = std::path::PathBuf::from(
        std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?,
    )
    .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut first = Tui::open(
        &runtime,
        [
            OsString::from("attach"),
            "--new".into(),
            "--".into(),
            powershell.into_os_string(),
            "-NoLogo".into(),
            "-NoProfile".into(),
        ],
    )?;
    first.wait_for("PS ")?;
    first.send(
        b"$env:CORVID_UPGRADE_VALUE='retained'; $env:CORVID_UPGRADE_PID=$PID; [Console]::WriteLine(('BE' + 'FORE_' + $PID))\r",
    )?;
    first.wait_for("BEFORE_")?;
    first.send(b"[Console]::Title='RETAINED_TITLE'\r")?;
    wait_for_title(&runtime, "RETAINED_TITLE", "RETAINED_TITLE").await?;
    first.send(b"\x1d.")?;
    first.wait_for("Rename tab")?;
    first.send(b"Retained tab\r")?;
    first.wait_without("Rename tab")?;
    wait_for_title(&runtime, "Retained tab", "RETAINED_TITLE").await?;
    let original_terminal = terminal_id(&runtime).await?;
    let output = tokio::process::Command::new(CORVID)
        .arg("--runtime-dir")
        .arg(&runtime)
        .arg("upgrade")
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _replacement = BackgroundDaemon::open(&runtime).await?;
    drop(old_daemon);
    drop(first);
    let current = corvid_client::host(&runtime).await?;
    ensure!(current.host_id == original.host_id && current.server_epoch != original.server_epoch);
    ensure!(terminal_id(&runtime).await? == original_terminal);
    wait_for_title(&runtime, "Retained tab", "RETAINED_TITLE").await?;
    let mut second = Tui::open(&runtime, [])?;
    second.wait_for("BEFORE_")?;
    second.send(b"[Console]::WriteLine(('AF' + 'TER_' + $PID + '_' + $env:CORVID_UPGRADE_VALUE)); [Console]::WriteLine(('SAME_' + 'PROCESS_' + ($env:CORVID_UPGRADE_PID -eq $PID)))\r")?;
    second.wait_for("_retained")?;
    second.wait_for("SAME_PROCESS_True")?;
    second.send(b"exit\r")?;
    second.expect_exit(0)?;
    Ok(())
}

#[tokio::test]
async fn managed_upgrade_replaces_the_windows_image_after_terminal_handoff() -> Result<()> {
    use corvid_protocol::windows::StateDirectory;

    let fixture = tempfile::tempdir()?;
    let runtime = fixture.path().join("run");
    let directory = StateDirectory::create_all(&runtime)?;
    let root = fixture.path().join("Corvid-SSH");
    let installation = StateDirectory::create_all(&root)?;
    let staged = root.join(".corvid-update.exe");
    let mut image = std::fs::read(CORVID)?;
    installation.replace_private_file("corvid.exe", &image)?;
    image.extend_from_slice(b"staged release");
    installation.replace_private_file(".corvid-update.exe", &image)?;
    let upgrade = || {
        Command::new(&staged)
            .env("LOCALAPPDATA", fixture.path())
            .arg("--runtime-dir")
            .arg(&runtime)
            .args(["upgrade", "--managed-ssh-from", env!("CARGO_PKG_VERSION")])
            .output()
    };

    let instance = directory.lock_instance()?;
    let refused = upgrade()?;
    assert!(!refused.status.success());
    assert_ne!(std::fs::read(root.join("corvid.exe"))?, image);

    drop(instance);
    let state = fixture.path().join("custom-state");
    let started = Command::new(root.join("corvid.exe"))
        .env("LOCALAPPDATA", fixture.path())
        .arg("--runtime-dir")
        .arg(&runtime)
        .args(["ensure-daemon", "--state-dir"])
        .arg(&state)
        .output()?;
    ensure!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let old_daemon = BackgroundDaemon::open(&runtime).await?;
    let old_host = corvid_client::host(&runtime).await?;
    let args = ["attach", "--new", "--", "cmd.exe", "/d", "/q"].map(OsString::from);
    let mut first = Tui::open(&runtime, args)?;
    first.wait_for("Microsoft Windows")?;
    first.send(b"set CORVID_MANAGED=READY\r")?;
    first.send(b"echo MANAGED_%CORVID_MANAGED%\r")?;
    first.wait_for("MANAGED_READY")?;
    let original_terminal = terminal_id(&runtime).await?;

    let updated = upgrade()?;
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let _daemon = BackgroundDaemon::open(&runtime).await?;
    drop(old_daemon);
    drop(first);
    assert_eq!(std::fs::read(root.join("corvid.exe"))?, image);
    assert_eq!(terminal_id(&runtime).await?, original_terminal);
    let new_host = corvid_client::host(&runtime).await?;
    assert_eq!(new_host.host_id, old_host.host_id);
    assert_ne!(new_host.server_epoch, old_host.server_epoch);
    let mut second = Tui::open(&runtime, [])?;
    second.wait_for("MANAGED_READY")?;
    second.send(b"exit\r")?;
    second.expect_exit(0)?;
    Ok(())
}

#[tokio::test]
async fn retains_the_windows_shell_across_tui_detach() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let runtime = fixture.path().join("run");
    let state = fixture.path().join("state");
    let _daemon = Daemon(
        Command::new(CORVID)
            .arg("--runtime-dir")
            .arg(&runtime)
            .args(["daemon", "--state-dir"])
            .arg(state)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    tokio::time::timeout(WAIT, async {
        while corvid_client::ping(&runtime).await.is_err() {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .context("Windows daemon did not answer ping")?;
    let host = corvid_client::host(&runtime).await?;

    let powershell = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .context("SystemRoot is unavailable")?
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let shell = [
        OsString::from("attach"),
        "--new".into(),
        "--".into(),
        powershell.into_os_string(),
        "-NoLogo".into(),
        "-NoProfile".into(),
    ];
    let mut first = Tui::open(&runtime, shell.clone())?;
    first.wait_for("PS ")?;
    // Windows PowerShell otherwise uses the host's legacy console code page.
    first.send(b"[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new()\r")?;
    first.send(
        b"$env:CORVID_TEST_VALUE='retained'; [Console]::WriteLine(('CORVID_' + [char]0xD55C))\r",
    )?;
    first.wait_for("CORVID_한")?;
    let original = terminal_id(&runtime).await?;

    let mut contender = Tui::open(&runtime, shell.clone())?;
    contender.wait_for("workspace already has a writer")?;
    contender.expect_exit(1)?;

    first.send(b"[Console]::WriteLine(('HISTORY_' + 'START')); 1..40 | ForEach-Object { [Console]::WriteLine(('HISTORY_' + $_)) }\r")?;
    first.wait_for("HISTORY_40")?;
    first.send(b"\x1dd")?;
    first.expect_exit(0)?;
    ensure!(
        terminal_id(&runtime).await? == original,
        "detach removed the live terminal"
    );
    tokio::time::timeout(WAIT, assert_history_after_detach(&runtime)).await??;

    let mut second = Tui::open(&runtime, [])?;
    second.wait_for("PS ")?;
    second.send(b"[Console]::WriteLine(('CORVID_' + $env:CORVID_TEST_VALUE))\r")?;
    second.wait_for("CORVID_retained")?;
    ensure!(
        terminal_id(&runtime).await? == original,
        "reattach changed the terminal ID"
    );
    let current = corvid_client::host(&runtime).await?;
    ensure!(
        current.server_epoch == host.server_epoch,
        "reattach changed the daemon epoch"
    );

    second.resize(92, 26)?;
    tokio::time::timeout(WAIT, async {
        loop {
            let snapshot = corvid_client::snapshot(&runtime).await?;
            if snapshot.snapshot().entities().any(|entity| {
                matches!(&entity.body, Some(Body::Client(client))
                    if client.active_viewport().is_some_and(|size| size.cols == 92 && size.rows == 26))
            }) {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .context("Windows daemon did not apply the TUI resize")??;
    second.send(b"[Console]::WriteLine(('CORVID_' + [Console]::WindowWidth + 'x' + [Console]::WindowHeight))\r")?;
    second.wait_for("CORVID_92x25")?;

    second.send(b"\x1dt")?;
    wait_for_layout(&runtime, 2, &original, false).await?;
    second.wait_for("[2:")?;
    second.send(b"[Console]::WriteLine(('SPLIT_' + 'SOURCE'))\r")?;
    second.wait_for("SPLIT_SOURCE")?;
    second.send(b"\x1d%")?;
    wait_for_layout(&runtime, 3, &original, false).await?;
    second.wait_without("SPLIT_SOURCE")?;
    second.send(b"\x1d1")?;
    wait_for_layout(&runtime, 3, &original, true).await?;
    second.wait_for("[1:")?;

    second.send(b"\x1d2")?;
    wait_for_layout(&runtime, 3, &original, false).await?;
    second.wait_for("[2:")?;
    second.send(b"[Console]::WriteLine(('CLOSE_' + 'SOURCE'))\r")?;
    second.wait_for("CLOSE_SOURCE")?;
    second.send(b"\x1dX")?;
    wait_for_layout(&runtime, 2, &original, false).await?;
    second.wait_without("CLOSE_SOURCE")?;
    second.send(b"\x1dT")?;
    wait_for_layout(&runtime, 1, &original, true).await?;
    second.wait_for("[1:")?;

    second.send(b"exit 7\r")?;
    second.expect_exit(7)?;

    let mut third = Tui::open(&runtime, shell)?;
    third.wait_for("PS ")?;
    third.send(b"\x1dS")?;
    third.expect_exit(0)?;
    let snapshot = corvid_client::snapshot(&runtime).await?;
    ensure!(
        !snapshot
            .snapshot()
            .entities()
            .any(|entity| matches!(entity.body, Some(Body::Terminal(_)))),
        "closing the session left a live terminal"
    );
    Ok(())
}

async fn assert_history_after_detach(runtime: &Path) -> Result<()> {
    use corvid_client::ControlEndpoint;
    use corvid_protocol::wire::envelope::Body;
    use corvid_protocol::wire::{Attach, AttachRole, Detach, ReadTerminalHistory, Viewport};
    use corvid_protocol::{Frame, TERMINAL_HISTORY_CAPABILITY, decode_envelope};

    let mut link = ControlEndpoint::Local(runtime.to_owned())
        .interactive(vec![TERMINAL_HISTORY_CAPABILITY.into()])
        .await?;
    link.send(Body::Attach(Attach {
        requested_role: AttachRole::Writer.into(),
        terminal_capabilities: vec!["legacy_vt.v1".into()],
        viewport: Some(Viewport {
            cols: COLS.into(),
            rows: ROWS.into(),
        }),
        ..Default::default()
    }))
    .await?;
    let Frame::Structured(reply) = link.receive().await? else {
        anyhow::bail!("expected attach reply");
    };
    let Some(Body::Attached(attached)) = decode_envelope(&reply)?.body else {
        anyhow::bail!("history client was not attached");
    };
    let revision = attached.route_token.context("missing route")?.view_revision;
    link.send(Body::ReadTerminalHistory(ReadTerminalHistory {
        expected_view_revision: Some(revision),
    }))
    .await?;
    loop {
        let Frame::Structured(reply) = link.receive().await? else {
            continue;
        };
        let reply = decode_envelope(&reply)?;
        let Some(Body::TerminalHistory(history)) = reply.body else {
            anyhow::bail!("unexpected history response: {:?}", reply.body);
        };
        let oldest = history
            .lines
            .iter()
            .position(|line| line == "HISTORY_START")
            .context("missing scrolled row")?;
        let newest = history
            .lines
            .iter()
            .position(|line| line == "HISTORY_40")
            .context("missing live row")?;
        assert!(oldest < newest);
        break;
    }
    link.send(Body::Detach(Detach {})).await?;
    while link.receive().await.is_ok() {}
    Ok(())
}

async fn wait_for_layout(
    runtime: &Path,
    terminal_count: usize,
    original: &str,
    original_selected: bool,
) -> Result<()> {
    tokio::time::timeout(WAIT, async {
        loop {
            let snapshot = corvid_client::snapshot(runtime).await?;
            let mut count = 0;
            let mut selected = None;
            for entity in snapshot.snapshot().entities() {
                match &entity.body {
                    Some(Body::Terminal(_)) => count += 1,
                    Some(Body::Client(client)) => selected = client.active_terminal(),
                    _ => {}
                }
            }
            if count == terminal_count
                && selected.is_some_and(|id| (id == original) == original_selected)
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .with_context(|| {
        format!(
            "Windows tab or pane transition did not finish: expected {terminal_count} terminals, original selected = {original_selected}"
        )
    })?
}

#[tokio::test]
async fn ensure_daemon_reuses_the_detached_windows_process() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let runtime = fixture.path().join("run");
    let state = fixture.path().join("state");
    let (first, second) = tokio::join!(
        ensure_daemon(&runtime, &state),
        ensure_daemon(&runtime, &state),
    );
    if first.is_err() && second.is_err() {
        return first;
    }
    let daemon = BackgroundDaemon::open(&runtime).await?;
    first?;
    second?;
    let host = corvid_client::host(&runtime).await?;
    ensure_daemon(&runtime, &state).await?;
    let reused = corvid_client::host(&runtime).await?;
    ensure!(
        reused.server_epoch == host.server_epoch,
        "ensure-daemon replaced a live daemon"
    );

    drop(daemon);
    ensure_daemon(&runtime, &state).await?;
    let _restarted = BackgroundDaemon::open(&runtime).await?;
    let restarted = corvid_client::host(&runtime).await?;
    ensure!(
        restarted.host_id == host.host_id,
        "restart changed the host identity"
    );
    ensure!(
        restarted.server_epoch != host.server_epoch,
        "restart reused a daemon epoch"
    );
    Ok(())
}

async fn ensure_daemon(runtime: &Path, state: &Path) -> Result<()> {
    let status = tokio::process::Command::new(CORVID)
        .arg("--runtime-dir")
        .arg(runtime)
        .args(["ensure-daemon", "--state-dir"])
        .arg(state)
        .status()
        .await?;
    ensure!(status.success(), "ensure-daemon failed ({status})");
    Ok(())
}

struct BackgroundDaemon(OwnedHandle);

impl BackgroundDaemon {
    async fn open(runtime: &Path) -> Result<Self> {
        let pipe = DaemonPipe::Control.connect(runtime).await?;
        let mut pid = 0;
        // The connected private pipe remains live while Windows reads its server PID.
        if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        // This PID comes from the isolated fixture's authenticated control pipe.
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        // OpenProcess returned an owned handle.
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle) }))
    }
}

impl Drop for BackgroundDaemon {
    fn drop(&mut self) {
        // The handle refers to this fixture's daemon even if its PID is recycled.
        unsafe {
            TerminateProcess(self.0.as_raw_handle(), 1);
            WaitForSingleObject(self.0.as_raw_handle(), 5000);
        }
    }
}

async fn terminal_id(runtime: &Path) -> Result<String> {
    let state = corvid_client::snapshot(runtime).await?;
    let mut ids = state
        .snapshot()
        .entities()
        .filter_map(|entity| match &entity.body {
            Some(Body::Terminal(terminal)) => Some(terminal.terminal_id.clone()),
            _ => None,
        });
    let id = ids.next().context("daemon has no live terminal")?;
    ensure!(ids.next().is_none(), "daemon has multiple live terminals");
    Ok(id)
}

struct Daemon(Process);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Tui {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    input: Box<dyn Write + Send>,
    output: Option<Receiver<Vec<u8>>>,
    screen: vt100::Parser,
}

impl Tui {
    fn open(runtime: &Path, args: impl IntoIterator<Item = OsString>) -> Result<Self> {
        Self::open_sized(runtime, args, COLS, ROWS)
    }

    fn open_sized(
        runtime: &Path,
        args: impl IntoIterator<Item = OsString>,
        cols: u16,
        rows: u16,
    ) -> Result<Self> {
        let pair = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut reader = pair.master.try_clone_reader()?;
        let input = pair.master.take_writer()?;
        let (sender, output) = sync_channel(16);
        std::thread::spawn(move || {
            let mut bytes = [0; 4096];
            let mut connected = true;
            while let Ok(count @ 1..) = reader.read(&mut bytes) {
                if connected {
                    connected = sender.send(bytes[..count].to_vec()).is_ok();
                }
            }
        });
        let mut command = CommandBuilder::new(CORVID);
        command.arg("--runtime-dir");
        command.arg(runtime);
        command.args(args);
        let child = pair.slave.spawn_command(command)?;
        let mut tui = Self {
            child,
            master: pair.master,
            input,
            output: Some(output),
            screen: vt100::Parser::new(
                NonZeroU16::new(rows).unwrap(),
                NonZeroU16::new(cols).unwrap(),
                0,
            ),
        };
        tui.answer_cursor_query()?;
        Ok(tui)
    }

    fn answer_cursor_query(&mut self) -> Result<()> {
        let deadline = Instant::now() + WAIT;
        let mut tail = Vec::new();
        loop {
            let bytes = self
                .read(deadline)
                .context("ConPTY did not request cursor position")?;
            tail.extend_from_slice(&bytes);
            if tail.windows(4).any(|window| window == b"\x1b[6n") {
                return self.send(b"\x1b[1;1R");
            }
            let discard = tail.len().saturating_sub(3);
            tail.drain(..discard);
        }
    }

    fn send(&mut self, input: &[u8]) -> Result<()> {
        self.input.write_all(input)?;
        self.input.flush()?;
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.master.resize(PtySize {
            cols,
            rows,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.screen.set_size(
            NonZeroU16::new(rows).unwrap(),
            NonZeroU16::new(cols).unwrap(),
        );
        Ok(())
    }

    fn read(&self, deadline: Instant) -> Result<Vec<u8>> {
        let time = deadline.saturating_duration_since(Instant::now());
        Ok(self.output.as_ref().unwrap().recv_timeout(time)?)
    }

    fn wait_for(&mut self, marker: &str) -> Result<()> {
        let deadline = Instant::now() + WAIT;
        while !self.screen.screen().contents().contains(marker) {
            let output = self.read(deadline).with_context(|| {
                format!(
                    "waiting for {marker}, test screen was {:?}",
                    self.screen.screen().contents()
                )
            })?;
            self.screen.process(&output);
        }
        Ok(())
    }

    // A daemon snapshot can change before the TUI retires its old input route.
    // Observe the replacement screen before sending another command.
    fn wait_without(&mut self, marker: &str) -> Result<()> {
        let deadline = Instant::now() + WAIT;
        while self.screen.screen().contents().contains(marker) {
            let output = self
                .read(deadline)
                .with_context(|| format!("waiting for the screen to retire {marker}"))?;
            self.screen.process(&output);
        }
        Ok(())
    }

    fn expect_exit(&mut self, expected: u32) -> Result<()> {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                ensure!(
                    status.exit_code() == expected,
                    "unexpected TUI exit status {status}"
                );
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "TUI did not exit");
            let _ = self.read(Instant::now() + Duration::from_millis(25));
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        drop(self.output.take());
    }
}
