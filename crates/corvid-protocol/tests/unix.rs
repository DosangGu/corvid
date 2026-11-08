#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use corvid_protocol::unix::{RuntimeDir, SocketRole};
use tempfile::TempDir;
use tokio::net::UnixListener;

fn test_parent() -> TempDir {
    // Keep paths short and avoid a world-writable ancestor.
    tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap()
}

fn path(parent: &TempDir) -> PathBuf {
    parent.path().join("runtime")
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn rejects_symlink_and_writable_runtime_ancestors() {
    let parent = test_parent();
    let actual = path(&parent);
    RuntimeDir::create(&actual).unwrap();
    let link = parent.path().join("link");
    symlink(&actual, &link).unwrap();

    assert!(RuntimeDir::open(&link).is_err());
    assert!(RuntimeDir::open(&actual.join("../runtime")).is_err());
    set_mode(parent.path(), 0o770);
    assert!(RuntimeDir::open(&actual).is_err());
    set_mode(parent.path(), 0o700);
    set_mode(&actual, 0o755);
    assert!(RuntimeDir::open(&actual).is_err());
}

#[tokio::test]
async fn authenticates_both_peers_and_holds_exclusive_lock_until_socket_closes() {
    let parent = test_parent();
    let path = path(&parent);
    let runtime = RuntimeDir::create(&path).unwrap();
    let instance = runtime.lock_instance().unwrap();
    let server = instance.bind(SocketRole::Control).await.unwrap();
    assert!(runtime.lock_instance().is_err());

    let socket_path = path.join("control.sock");
    assert_eq!(
        fs::metadata(&socket_path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let client = RuntimeDir::open(&path).unwrap();
    let (client, peer) = tokio::join!(client.connect(SocketRole::Control), server.accept());
    assert!(client.is_ok());
    assert!(peer.unwrap().is_some());

    drop(server);
    assert!(!socket_path.exists());
    drop(instance);
    assert!(runtime.lock_instance().is_ok());
}

#[tokio::test]
async fn preserves_live_socket_and_replaces_only_stale_owned_socket() {
    let parent = test_parent();
    let path = path(&parent);
    let runtime = RuntimeDir::create(&path).unwrap();
    let socket_path = path.join("control.sock");
    let live = UnixListener::bind(&socket_path).unwrap();
    set_mode(&socket_path, 0o600);

    let instance = runtime.lock_instance().unwrap();
    assert!(instance.bind(SocketRole::Control).await.is_err());
    assert!(socket_path.exists());
    assert!(live.accept().await.is_ok());
    drop(live);

    let replacement = instance.bind(SocketRole::Control).await.unwrap();
    assert!(runtime.connect(SocketRole::Control).await.is_ok());
    drop(replacement);

    fs::write(&socket_path, "retain this file").unwrap();
    assert!(instance.bind(SocketRole::Control).await.is_err());
    assert_eq!(fs::read_to_string(socket_path).unwrap(), "retain this file");
}
