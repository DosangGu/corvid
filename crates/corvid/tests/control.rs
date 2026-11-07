#![cfg(target_os = "linux")]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;

use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{GetHost, HostInfo};
use corvid_protocol::{FrameReader, FrameRole, FrameWriter, PeerRole, check_hello_result, hello};
use tokio::time::timeout;

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
