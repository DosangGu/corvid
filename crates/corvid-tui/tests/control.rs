#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use corvid_async::io::{AsyncReadExt, AsyncWriteExt};
use corvid_async::time::{sleep, timeout};
use corvid_protocol::collector::{
    ANTIGRAVITY_VERSION, CAPABILITY as COLLECTOR_CAPABILITY, CLAUDE_VERSION, CODEX_VERSION,
    HOST_ID, LAUNCH_ASSOCIATION, SERVER_EPOCH,
};
use corvid_protocol::snapshot::{SnapshotAssembler, WATCH_CAPABILITY};
use corvid_protocol::unix::{RuntimeDir, SocketRole};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    AgentProvider, Attach, AttachRole, CollectHook, CollectorStatus, CollectorTraffic, Detach,
    GetCapabilities, GetHost, HookEvent, HostInfo, LaunchCommand, Ping, ProviderCapabilityKind,
    ProviderExecutionMode, ProviderQualification, SnapshotWatch, Viewport,
};
use corvid_protocol::{
    CAPABILITY_QUERY, CLIPBOARD_WRITE_CAPABILITY, FRAME_PREAMBLE, Frame, FrameReader, FrameRole,
    FrameWriter, PROTOCOL_MAJOR, PeerRole, check_hello_result, hello,
};
use uuid::Uuid;

use common::{WAIT, command, daemon, start_daemon};

#[compio::test]
async fn relays_daemon_terminal_frames_without_an_outer_tty() {
    let temporary = tempfile::tempdir_in(std::env::home_dir().unwrap()).unwrap();
    let runtime = temporary.path().join("runtime");
    let _daemon = daemon(&runtime, &temporary.path().join("state")).await;

    timeout(WAIT, async {
        let mut relay = command(&runtime)
            .arg("terminal-relay")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut writer = FrameWriter::start(relay.stdin.take().unwrap(), FrameRole::StructuredOnly)
            .await
            .unwrap();
        let mut reader = FrameReader::start(relay.stdout.take().unwrap(), FrameRole::TuiOutput)
            .await
            .unwrap();
        let greeting = hello(
            "greeting".into(),
            None,
            vec!["attach.v1".into(), "render.v1".into()],
        );
        writer.envelope(&greeting).await.unwrap();
        let session = check_hello_result(
            &greeting,
            &reader.next_envelope().await.unwrap().unwrap(),
            PeerRole::Tui,
        )
        .unwrap();
        let request = session.envelope(
            Some("attach".into()),
            Body::Attach(Attach {
                viewport: Some(Viewport { rows: 24, cols: 80 }),
                terminal_capabilities: vec!["legacy_vt.v1".into(), CLIPBOARD_WRITE_CAPABILITY.into()],
                requested_role: AttachRole::Writer as i32,
                launch: Some(LaunchCommand {
                    argv: vec![
                        b"/bin/sh".to_vec(),
                        b"-c".to_vec(),
                        b"printf '\\033[?1049h\\033[?1000h\\033[?1006hDAEMON-RELAY-READY\\n'; read answer; printf '\\033]52;c;aGVsbG8K7ZWc6riA\\007'; read again".to_vec(),
                    ],
                    ..Default::default()
                }),
                ..Default::default()
            }),
        );
        writer.envelope(&request).await.unwrap();
        let reply = reader.next_envelope().await.unwrap().unwrap();
        let Some(Body::Attached(attached)) = reply.body else {
            panic!("expected attachment");
        };
        loop {
            let Frame::Render(frame) = reader.next_frame().await.unwrap().unwrap() else {
                panic!("expected ANSI render")
            };
            if String::from_utf8_lossy(&frame.ansi).contains("DAEMON-RELAY-READY") {
                assert!(frame.header.alternate_screen.is_some());
                assert_eq!(frame.header.wheel_encoding(), corvid_protocol::wire::WheelEncoding::Sgr);
                break;
            }
            let applied = corvid_protocol::wire::FrameApplied {
                render_generation: frame.header.render_generation,
                frame_id: frame.header.frame_id,
            };
            writer
                .envelope(&session.envelope(Some("applied".into()), Body::FrameApplied(applied)))
                .await
                .unwrap();
        }
        writer
            .envelope(&session.envelope(
                Some("copy".into()),
                Body::InputChunk(corvid_protocol::wire::InputChunk {
                    route_token: attached.route_token.clone(),
                    input_sequence: 1,
                    data: b"copy\n".to_vec(),
                }),
            ))
            .await
            .unwrap();
        loop {
            match reader.next_frame().await.unwrap().unwrap() {
                Frame::Render(frame) => {
                    assert!(!frame.ansi.windows(4).any(|bytes| bytes == b"\x1b]52"));
                }
                Frame::Structured(bytes) => {
                    let envelope = corvid_protocol::decode_envelope(&bytes).unwrap();
                    let Some(Body::ClipboardWrite(write)) = envelope.body else {
                        panic!("expected clipboard write");
                    };
                    assert_eq!(write.route_token, attached.route_token);
                    assert_eq!(write.text, "hello\n한글");
                    break;
                }
            }
        }
        writer
            .envelope(&session.envelope(Some("detach".into()), Body::Detach(Detach {})))
            .await
            .unwrap();
        writer.shutdown().await.unwrap();
        drop(writer);
        assert!(relay.status().await.unwrap().success());
        let status = corvid_client::status(&runtime).await.unwrap();
        assert!(status.terminal_live);
    })
    .await
    .unwrap();
}

async fn local_host(runtime: &Path) -> String {
    let output = timeout(WAIT, command(runtime).args(["control", "host"]).output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[compio::test]
async fn daemon_restart_preserves_host_identity_and_changes_epoch() {
    let temporary = tempfile::tempdir_in(std::env::home_dir().unwrap()).unwrap();
    let runtime = temporary.path().join("runtime");
    let state = temporary.path().join("state/corvid");
    let mut first = daemon(&runtime, &state).await;
    let first_identity = local_host(&runtime).await;

    let identity_file = state.join("identity.json");
    let permissions = fs::metadata(&identity_file).unwrap().permissions().mode();
    assert_eq!(permissions & 0o777, 0o600);
    let duplicate = command(&runtime)
        .arg("daemon")
        .arg("--state-dir")
        .arg(&state)
        .output()
        .await
        .unwrap();
    assert!(!duplicate.status.success());
    assert_eq!(first_identity, local_host(&runtime).await);

    first.kill().unwrap();
    first.status().await.unwrap();
    let mut second = daemon(&runtime, &state).await;
    let second_identity = local_host(&runtime).await;
    assert_eq!(
        first_identity.lines().next(),
        second_identity.lines().next()
    );
    assert_ne!(
        first_identity.lines().nth(1),
        second_identity.lines().nth(1)
    );
    second.kill().unwrap();
    second.status().await.unwrap();

    fs::write(&identity_file, b"{incomplete").unwrap();
    let result = command(&runtime)
        .arg("daemon")
        .arg("--state-dir")
        .arg(&state)
        .output()
        .await
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(fs::read(identity_file).unwrap(), b"{incomplete");
}

#[compio::test]
async fn relays_real_control_session_over_binary_stdio() {
    let temporary = tempfile::tempdir_in(std::env::home_dir().unwrap()).unwrap();
    let runtime = temporary.path().join("runtime");
    let probe = temporary.path().join("provider-launched");
    for provider in ["claude", "codex"] {
        let executable = temporary.path().join(provider);
        fs::write(
            &executable,
            b"#!/bin/sh\nprintf 'launched\\n' > \"$CORVID_TEST_PROBE\"\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut daemon_command = command(&runtime);
    daemon_command
        .env("PATH", temporary.path())
        .env("CORVID_TEST_PROBE", &probe);
    let _daemon = start_daemon(daemon_command, &runtime, &temporary.path().join("state")).await;

    timeout(WAIT, async {
        let mut relay = command(&runtime)
            .args(["control", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut writer = FrameWriter::start(relay.stdin.take().unwrap(), FrameRole::StructuredOnly)
            .await
            .unwrap();
        let mut reader =
            FrameReader::start(relay.stdout.take().unwrap(), FrameRole::StructuredOnly)
                .await
                .unwrap();

        let hello = hello(
            "greeting".into(),
            Some("test-controller".into()),
            vec!["host.v1".into(), CAPABILITY_QUERY.into()],
        );
        writer.envelope(&hello).await.unwrap();
        let reply = reader.next_envelope().await.unwrap().unwrap();
        let session = check_hello_result(&hello, &reply, PeerRole::Control).unwrap();
        let request = session.envelope(
            Some("features".into()),
            Body::GetCapabilities(GetCapabilities {}),
        );
        writer.envelope(&request).await.unwrap();
        let reply = reader.next_envelope().await.unwrap().unwrap();
        session.validate_scope(&reply).unwrap();
        assert_eq!(reply.request_id.as_deref(), Some("features"));
        assert_eq!(reply.body, Some(Body::Capabilities(session.description())));
        verify_qualified_profiles(&session.provider_profiles);

        let request = session.envelope(Some("host".into()), Body::GetHost(GetHost {}));
        writer.envelope(&request).await.unwrap();
        writer.shutdown().await.unwrap();
        drop(writer);
        let reply = reader.next_envelope().await.unwrap().unwrap();
        assert_eq!(reply.request_id.as_deref(), Some("host"));
        assert_eq!(
            reply.body,
            Some(Body::HostInfo(HostInfo {
                host_id: session.host_id,
                server_epoch: session.server_epoch,
            }))
        );
        assert!(reader.next_envelope().await.unwrap().is_none());
        assert!(relay.status().await.unwrap().success());

        let output = command(&runtime)
            .args(["control", "capabilities"])
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(output.contains(&format!(
            "provider=Claude qualified_version={CLAUDE_VERSION}"
        )));
        assert!(output.contains(&format!("provider=Codex qualified_version={CODEX_VERSION}")));
        assert!(output.contains(&format!(
            "provider=Antigravity qualified_version={ANTIGRAVITY_VERSION}"
        )));
        assert!(output.contains("observation=ExplicitQuestion qualification=Partial"));
        assert!(
            corvid_client::agents(&runtime)
                .await
                .unwrap()
                .agents
                .is_empty()
        );
        assert!(
            !probe.exists(),
            "capability discovery launched a provider executable"
        );
    })
    .await
    .unwrap();
}

fn verify_qualified_profiles(profiles: &[corvid_protocol::wire::ProviderProfile]) {
    assert_eq!(profiles.len(), 3);
    for (provider, version) in [
        (AgentProvider::Claude, CLAUDE_VERSION),
        (AgentProvider::Codex, CODEX_VERSION),
        (AgentProvider::Antigravity, ANTIGRAVITY_VERSION),
    ] {
        let profile = profiles
            .iter()
            .find(|profile| profile.provider == provider as i32)
            .unwrap();
        assert_eq!(profile.version, version);
        let adapter = profile.adapter.as_ref().unwrap();
        assert_eq!(adapter.mode, ProviderExecutionMode::InteractiveCli as i32);
        let expected_question = if provider == AgentProvider::Antigravity {
            ProviderQualification::Unavailable
        } else {
            ProviderQualification::Partial
        };
        assert!(adapter.capabilities.iter().any(|capability| {
            capability.kind == ProviderCapabilityKind::ExplicitQuestion as i32
                && capability.qualification == expected_question as i32
        }));
    }
}

#[compio::test]
async fn collector_status_separates_rejected_submissions_from_connection_failures() {
    let temporary = tempfile::tempdir_in(std::env::home_dir().unwrap()).unwrap();
    let runtime = temporary.path().join("runtime");
    let state = temporary.path().join("state");
    let mut first = daemon(&runtime, &state).await;
    let empty = CollectorStatus {
        traffic: Some(CollectorTraffic::default()),
        ..Default::default()
    };
    let directory = RuntimeDir::open(&runtime).unwrap();

    timeout(WAIT, async {
        assert_eq!(
            corvid_client::status(&runtime)
                .await
                .unwrap()
                .collector
                .as_ref(),
            Some(&empty)
        );
        let peer = directory.connect(SocketRole::Collector).await.unwrap();
        let reader = FrameReader::start(corvid_async::io::adapt(peer), FrameRole::Collector)
            .await
            .unwrap();
        drop(reader);
        loop {
            let status = corvid_client::status(&runtime)
                .await
                .unwrap()
                .collector
                .unwrap();
            if status.traffic.unwrap().connection_failures == 1 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        reject_unowned_hook(&directory).await;
        let status = corvid_client::status(&runtime)
            .await
            .unwrap()
            .collector
            .unwrap();
        assert_eq!(
            status,
            CollectorStatus {
                traffic: Some(CollectorTraffic {
                    connection_failures: 1,
                    rejected_submissions: 1,
                    ..Default::default()
                }),
                ..Default::default()
            }
        );
        assert!(
            corvid_client::agents(&runtime)
                .await
                .unwrap()
                .agents
                .is_empty()
        );
    })
    .await
    .unwrap();
    first.kill().unwrap();
    first.status().await.unwrap();
    let _second = daemon(&runtime, &state).await;
    assert_eq!(
        corvid_client::status(&runtime).await.unwrap().collector,
        Some(empty)
    );
}

async fn reject_unowned_hook(directory: &RuntimeDir) {
    let (read, write) =
        corvid_async::io::split_native(directory.connect(SocketRole::Collector).await.unwrap());
    let mut writer = FrameWriter::start(write, FrameRole::Collector)
        .await
        .unwrap();
    let mut reader = FrameReader::start(read, FrameRole::Collector)
        .await
        .unwrap();
    let greeting = hello("collector".into(), None, vec![COLLECTOR_CAPABILITY.into()]);
    writer.envelope(&greeting).await.unwrap();
    let reply = reader.next_envelope().await.unwrap().unwrap();
    let session = check_hello_result(&greeting, &reply, PeerRole::Collector).unwrap();

    let observation = Body::CollectHook(CollectHook {
        launch_association: Uuid::new_v4().to_string(),
        provider: AgentProvider::Claude as i32,
        event: HookEvent::Startup as i32,
        session_id: Some(Uuid::new_v4().to_string()),
        ..Default::default()
    });
    let request = session.envelope(Some(Uuid::new_v4().to_string()), observation);
    writer.envelope(&request).await.unwrap();
    let reply = reader.next_envelope().await.unwrap().unwrap();
    assert!(matches!(reply.body, Some(Body::Error(fault)) if fault.code == "Unauthorized"));
}

#[compio::test]
async fn relays_a_complete_watch_setup_and_heartbeat_over_stdio() {
    let temporary = tempfile::tempdir_in(std::env::home_dir().unwrap()).unwrap();
    let runtime = temporary.path().join("runtime");
    let _daemon = daemon(&runtime, &temporary.path().join("state")).await;

    timeout(WAIT, async {
        let mut relay = command(&runtime)
            .args(["control", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut writer = FrameWriter::start(relay.stdin.take().unwrap(), FrameRole::StructuredOnly)
            .await
            .unwrap();
        let mut reader =
            FrameReader::start(relay.stdout.take().unwrap(), FrameRole::StructuredOnly)
                .await
                .unwrap();
        let greeting = hello(
            "hello".into(),
            Some("watch-controller".into()),
            vec![WATCH_CAPABILITY.into()],
        );
        writer.envelope(&greeting).await.unwrap();
        let reply = reader.next_envelope().await.unwrap().unwrap();
        let session = check_hello_result(&greeting, &reply, PeerRole::Control).unwrap();

        let mut assembler = SnapshotAssembler::new(session.clone(), "watch".to_owned(), true);
        let request = session.envelope(
            Some("watch".into()),
            Body::SnapshotWatch(SnapshotWatch::default()),
        );
        writer.envelope(&request).await.unwrap();
        let snapshot = loop {
            let frame = reader.next_envelope().await.unwrap().unwrap();
            if let Some(snapshot) = assembler.push(frame).unwrap() {
                break snapshot;
            }
        };
        assert_eq!(snapshot.snapshot().sequence(), 0);
        assert_eq!(snapshot.snapshot().entities().count(), 1);
        assert!(snapshot.subscription_id().is_some());

        let ping = session.envelope(Some("ping".into()), Body::Ping(Ping {}));
        writer.envelope(&ping).await.unwrap();
        let pong = reader.next_envelope().await.unwrap().unwrap();
        assert!(matches!(pong.body, Some(Body::Pong(_))));
        assert_eq!(pong.request_id, ping.request_id);
        assert!(pong.subscription_id.is_none());

        writer.shutdown().await.unwrap();
        drop(writer);
        assert!(reader.next_envelope().await.unwrap().is_none());
        assert!(relay.status().await.unwrap().success());
    })
    .await
    .unwrap();
}

#[compio::test]
async fn relay_reports_unavailable_without_creating_runtime_directory() {
    let temporary = tempfile::tempdir_in(std::env::home_dir().unwrap()).unwrap();
    let runtime = temporary.path().join("missing");
    timeout(WAIT, async {
        let mut relay = command(&runtime)
            .args(["control", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut writer = FrameWriter::start(relay.stdin.take().unwrap(), FrameRole::StructuredOnly)
            .await
            .unwrap();
        let mut reader =
            FrameReader::start(relay.stdout.take().unwrap(), FrameRole::StructuredOnly)
                .await
                .unwrap();
        let greeting = hello(
            "unavailable".into(),
            Some("test-controller".into()),
            Vec::new(),
        );
        writer.envelope(&greeting).await.unwrap();
        let reply = reader.next_envelope().await.unwrap().unwrap();
        let error = check_hello_result(&greeting, &reply, PeerRole::Control).unwrap_err();
        assert_eq!(error.code, "DaemonUnavailable");
        assert!(!relay.status().await.unwrap().success());
        assert!(!runtime.exists());
    })
    .await
    .unwrap();
}

#[compio::test]
async fn incompatible_daemon_framing_is_reported_locally_and_through_the_relay() {
    let temporary = tempfile::tempdir_in(std::env::home_dir().unwrap()).unwrap();
    let runtime = temporary.path().join("runtime");
    let directory = RuntimeDir::create(&runtime).unwrap();
    let instance = directory.lock_instance().unwrap();
    let socket = instance.bind(SocketRole::Control).await.unwrap();
    let legacy = async {
        for _ in 0..2 {
            let peer = socket.accept().await.unwrap().unwrap();
            let mut peer = corvid_async::io::adapt(peer);
            peer.write_all(b"CRV0").await.unwrap();
            peer.flush().await.unwrap();

            let mut received = Vec::new();
            peer.read_to_end(&mut received).await.unwrap();
            assert_eq!(received, FRAME_PREAMBLE);
        }
    };

    let client = async {
        let local = command(&runtime)
            .args(["control", "host"])
            .output()
            .await
            .unwrap();
        assert!(!local.status.success());
        assert!(local.stdout.is_empty());
        let stderr = String::from_utf8(local.stderr).unwrap();
        assert!(stderr.contains("ProtocolMismatch") && stderr.contains("unsupported framing"));

        let mut relay = command(&runtime)
            .args(["control", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut writer = FrameWriter::start(relay.stdin.take().unwrap(), FrameRole::StructuredOnly)
            .await
            .unwrap();
        let mut reader =
            FrameReader::start(relay.stdout.take().unwrap(), FrameRole::StructuredOnly)
                .await
                .unwrap();
        let greeting = hello("legacy".into(), Some("test-controller".into()), vec![]);
        writer.envelope(&greeting).await.unwrap();
        let reply = reader.next_envelope().await.unwrap().unwrap();
        let fault = check_hello_result(&greeting, &reply, PeerRole::Control).unwrap_err();
        assert_eq!(fault.code, "ProtocolMismatch");
        assert_eq!(fault.supported_major_versions, [PROTOCOL_MAJOR]);
        assert!(!fault.retryable);
        assert!(reader.next_envelope().await.unwrap().is_none());
        assert!(!relay.status().await.unwrap().success());
    };
    timeout(WAIT, async { corvid_async::join!(legacy, client) })
        .await
        .unwrap();
}

#[compio::test]
async fn observers_exit_neutrally_with_missing_daemon_and_bad_or_stalled_input() {
    let temporary = tempfile::tempdir_in(std::env::home_dir().unwrap()).unwrap();
    let runtime = temporary.path().join("missing");
    let valid = br#"{"hook_event_name":"SessionStart","source":"startup","session_id":"session"}"#;
    let oversized = vec![b'x'; 65 * 1024];
    for (args, expected) in [
        (&["observe", "--provider", "claude"][..], b"".as_slice()),
        (
            &["observe", "--provider", "codex", "--neutral-json"][..],
            b"{}\n".as_slice(),
        ),
    ] {
        for (input, close_stdin) in [
            (valid.as_slice(), true),
            (b"{".as_slice(), true),
            (oversized.as_slice(), true),
            (b"{".as_slice(), false),
        ] {
            let mut child = command(&runtime)
                .args(args)
                .env(HOST_ID, Uuid::new_v4().to_string())
                .env(SERVER_EPOCH, Uuid::new_v4().to_string())
                .env(LAUNCH_ASSOCIATION, Uuid::new_v4().to_string())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let write = timeout(Duration::from_millis(250), stdin.write_all(input))
                .await
                .unwrap();
            // An oversized pipe can close as soon as the helper reaches its bound.
            if input.len() < 64 * 1024 {
                write.unwrap();
            }
            let _held_open = (!close_stdin).then_some(stdin);
            let output = timeout(Duration::from_millis(250), child.output())
                .await
                .unwrap()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, expected);
            assert!(output.stderr.is_empty());
            assert!(!runtime.exists());
        }
    }
}

#[compio::test]
async fn plain_tui_falls_back_to_default_runtime_when_saved_runtime_is_unreachable() {
    use corvid_client::view_state::{Target, ViewStore};
    use corvid_protocol::wire::ViewSelection;

    let temporary = tempfile::tempdir_in(std::env::home_dir().unwrap()).unwrap();
    let root = temporary.path();
    let xdg_runtime = root.join("xdg-run");
    let xdg_state = root.join("xdg-state");
    let home = root.join("home");
    let views = root.join("views");
    fs::create_dir_all(&xdg_runtime).unwrap();
    fs::create_dir_all(&xdg_state).unwrap();
    fs::create_dir_all(&home).unwrap();

    #[cfg(target_os = "linux")]
    let default_runtime = xdg_runtime.join("corvid");
    #[cfg(target_os = "macos")]
    let default_runtime = {
        let boot = corvid_protocol::macos::boot_session().unwrap();
        home.join(".corvid").join(format!("r-{}", &boot[..16]))
    };

    #[cfg(target_os = "linux")]
    let default_state = xdg_state.join("corvid");
    #[cfg(target_os = "macos")]
    let default_state = home.join("Library/Application Support/Corvid/daemon");

    let _default_daemon = daemon(&default_runtime, &default_state).await;

    let save_tui_runtime = |runtime: &Path| {
        let store = ViewStore::new(views.join("tui"));
        let recorder = store.recorder().unwrap();
        recorder.record(
            Target::Tui {
                runtime: runtime.to_path_buf(),
            },
            &Uuid::new_v4().to_string(),
            &ViewSelection {
                workspace_id: Uuid::new_v4().to_string(),
                tab_id: Uuid::new_v4().to_string(),
                pane_id: Uuid::new_v4().to_string(),
            },
        );
        drop(recorder);
    };

    let run_plain_corvid = |args: &[&str]| {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_corvid"));
        cmd.args(args)
            .env("HOME", &home)
            .env("XDG_RUNTIME_DIR", &xdg_runtime)
            .env("XDG_STATE_HOME", &xdg_state)
            .env("CORVID_VIEW_STATE_DIR", &views)
            .env_remove("CORVID_RUNTIME_DIR")
            .stdin(Stdio::null());
        cmd.output().unwrap()
    };

    // 1. Deleted saved runtime (and its parent directory) must not be recreated or fail.
    let stale_parent = root.join("deleted-parent");
    let stale_runtime = stale_parent.join("stale-run");
    fs::create_dir_all(&stale_runtime).unwrap();
    save_tui_runtime(&stale_runtime);
    fs::remove_dir_all(&stale_parent).unwrap();

    let output = run_plain_corvid(&["list"]);
    assert!(
        output.status.success(),
        "plain corvid list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!stale_parent.exists());
    assert!(!stale_runtime.exists());

    // 2. Existing but dead runtime directory must also fall back to the default daemon.
    let dead_runtime = root.join("dead-run");
    fs::create_dir_all(&dead_runtime).unwrap();
    save_tui_runtime(&dead_runtime);
    let output = run_plain_corvid(&["list"]);
    assert!(
        output.status.success(),
        "plain corvid list failed for dead runtime: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // 3. Authoritative overrides (--runtime-dir and CORVID_RUNTIME_DIR) must not fall back.
    let missing_override = root.join("missing-override");
    let cli_override = std::process::Command::new(env!("CARGO_BIN_EXE_corvid"))
        .arg("--runtime-dir")
        .arg(&missing_override)
        .arg("list")
        .env("HOME", &home)
        .env("XDG_RUNTIME_DIR", &xdg_runtime)
        .env("XDG_STATE_HOME", &xdg_state)
        .env("CORVID_VIEW_STATE_DIR", &views)
        .env_remove("CORVID_RUNTIME_DIR")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!cli_override.status.success());
    assert!(!missing_override.exists());

    let env_override = std::process::Command::new(env!("CARGO_BIN_EXE_corvid"))
        .arg("list")
        .env("HOME", &home)
        .env("XDG_RUNTIME_DIR", &xdg_runtime)
        .env("XDG_STATE_HOME", &xdg_state)
        .env("CORVID_VIEW_STATE_DIR", &views)
        .env("CORVID_RUNTIME_DIR", &missing_override)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!env_override.status.success());
    assert!(!missing_override.exists());

    // 4. Reachable remembered runtime continues to be selected by plain corvid.
    let custom_runtime = root.join("custom-run");
    let custom_state = root.join("custom-state");
    let custom_daemon = daemon(&custom_runtime, &custom_state).await;
    save_tui_runtime(&custom_runtime);
    let output = run_plain_corvid(&["list"]);
    assert!(
        output.status.success(),
        "plain corvid list failed for reachable remembered runtime: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    drop(custom_daemon);
}
