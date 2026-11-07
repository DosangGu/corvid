#![cfg(target_os = "linux")]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use corvid_protocol::collector::{HOST_ID, LAUNCH_ASSOCIATION, SERVER_EPOCH};
use corvid_protocol::snapshot::{SnapshotAssembler, WATCH_CAPABILITY};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{GetHost, HostInfo, Ping, SnapshotWatch};
use corvid_protocol::{FrameReader, FrameRole, FrameWriter, PeerRole, check_hello_result, hello};
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;
use uuid::Uuid;

use common::{WAIT, command, daemon};

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

#[tokio::test]
async fn daemon_restart_preserves_host_identity_and_changes_epoch() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
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

    first.kill().await.unwrap();
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
    second.kill().await.unwrap();

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

#[tokio::test]
async fn relays_real_control_session_over_binary_stdio() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
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

        let hello = hello(
            "greeting".into(),
            Some("test-controller".into()),
            vec!["host.v1".into()],
        );
        writer.envelope(&hello).await.unwrap();
        let reply = reader.next_envelope().await.unwrap().unwrap();
        let session = check_hello_result(&hello, &reply, PeerRole::Control).unwrap();
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
        assert!(relay.wait().await.unwrap().success());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn relays_a_complete_watch_setup_and_heartbeat_over_stdio() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
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
        let request = session.envelope(Some("watch".into()), Body::SnapshotWatch(SnapshotWatch {}));
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
        assert!(relay.wait().await.unwrap().success());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn relay_reports_unavailable_without_creating_runtime_directory() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
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
        assert!(!relay.wait().await.unwrap().success());
        assert!(!runtime.exists());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn observe_exits_silently_with_missing_daemon_and_bad_or_stalled_input() {
    let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let runtime = temporary.path().join("missing");
    let valid = br#"{"hook_event_name":"SessionStart","source":"startup","session_id":"session"}"#;
    let oversized = vec![b'x'; 65 * 1024];
    for (input, close_stdin) in [
        (valid.as_slice(), true),
        (b"{".as_slice(), true),
        (oversized.as_slice(), true),
        (b"{".as_slice(), false),
    ] {
        let mut child = command(&runtime)
            .args(["observe", "--provider", "claude"])
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
        // The helper can close an oversized pipe as soon as it reaches the bound.
        if input.len() < 64 * 1024 {
            write.unwrap();
        }
        let _held_open = (!close_stdin).then_some(stdin);
        let output = timeout(Duration::from_millis(250), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        assert!(!runtime.exists());
    }
}
