use super::*;

#[test]
fn compares_release_precedence_without_downgrading() {
    let current = env!("CARGO_PKG_VERSION");
    assert!(bundled_is_newer("0.1.2").unwrap());
    assert!(!bundled_is_newer(current).unwrap());
    assert!(!bundled_is_newer(&format!("{current}+repacked")).unwrap());
    assert!(!bundled_is_newer("999.0.0").unwrap());
    assert!(bundled_is_newer("not-a-version").is_err());
}

// The fixture supplies an isolated managed installation, real daemon/relay, and
// a local SSH command shim. It records upgrade dispatch instead of replacing
// the daemon; daemon handoff itself is covered by the runtime lifecycle tests.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "run with deploy/test_remote_update.py"]
async fn connected_managed_installations_offer_updates() -> Result<()> {
    use corvid_async::mpsc;
    use corvid_client::SshAlias;

    let root = std::path::PathBuf::from(std::env::var("CORVID_REMOTE_UPDATE_FIXTURE")?);
    let version = root.join("installed-version");
    let queried = root.join("version-queried");
    let upgraded = root.join("upgraded.json");
    let host = RemoteHost::Ssh(SshAlias::new("corvid-update-test".into())?);
    let connect = |explicit, expected: Option<String>, decision| {
        let host = host.clone();
        async move {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let prompts = Prompts(sender);
            let connection = super::connect(
                host,
                vec!["attach.v1".into(), "render.v1".into()],
                explicit,
                expected.as_deref(),
                &prompts,
                |_| {},
            );
            corvid_async::pin!(connection);
            let mut messages = Vec::new();
            loop {
                corvid_async::select! {
                    result = &mut connection => return (result, messages),
                    Some(prompt) = receiver.recv() => {
                        messages.push((prompt.action, prompt.message));
                        let _ = prompt.decision.send(decision);
                    }
                }
            }
        }
    };

    std::fs::write(&version, "0.1.2")?;
    let (result, messages) = connect(true, None, false).await;
    let (_, link) = result?;
    let host_id = link.session().host_id.clone();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0, Some("Upgrade"));
    assert!(messages[0].1.contains("0.1.2 →"));
    assert!(messages[0].1.contains("Cancel to keep this version."));
    assert!(queried.exists());
    assert!(!upgraded.exists());
    drop(link);

    // Reconnects and PATH installations must not prompt or inspect the version.
    for path_installation in [false, true] {
        if path_installation {
            std::fs::write(root.join("path-installation"), "")?;
        }
        std::fs::remove_file(&queried)?;
        let (result, messages) = connect(path_installation, Some(host_id.clone()), true).await;
        result?;
        assert!(messages.is_empty());
        assert!(!queried.exists());
        // Keep reset logic the same for both iterations.
        std::fs::write(&queried, "")?;
    }
    std::fs::remove_file(root.join("path-installation"))?;
    std::fs::remove_file(&queried)?;

    // Verify the saved host before querying or changing an installation.
    let (result, messages) = connect(true, Some(Uuid::new_v4().to_string()), true).await;
    assert!(has_fault(
        &result.err().context("accepted a different host")?,
        "HostMismatch"
    ));
    assert!(messages.is_empty());
    assert!(!queried.exists());

    for installed in [env!("CARGO_PKG_VERSION"), "999.0.0"] {
        std::fs::write(&version, installed)?;
        let (result, messages) = connect(true, Some(host_id.clone()), true).await;
        result?;
        assert!(messages.is_empty());
        assert!(!upgraded.exists());
    }

    // A failed check or bundle validation reports the problem and keeps the link.
    std::fs::write(&version, "invalid")?;
    let (result, messages) = connect(true, Some(host_id.clone()), false).await;
    result?;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0, None);
    std::fs::write(&version, "0.1.2")?;
    let bundle = root.join("bundle-file");
    let bundle = std::path::PathBuf::from(std::fs::read_to_string(bundle)?);
    let original = std::fs::read(&bundle)?;
    std::fs::write(&bundle, "invalid bundle")?;
    let (result, messages) = connect(true, Some(host_id.clone()), true).await;
    result?;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0, None);
    assert!(!upgraded.exists());
    std::fs::write(&bundle, original)?;

    let (result, messages) = connect(true, Some(host_id.clone()), true).await;
    let (_, link) = result?;
    assert_eq!(link.session().host_id, host_id);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0, Some("Upgrade"));
    let dispatch: serde_json::Value = serde_json::from_slice(&std::fs::read(upgraded)?)?;
    assert_eq!(dispatch["previous_version"], "0.1.2");
    assert_eq!(dispatch["expected_host"], host_id);
    assert_eq!(dispatch["verified_payload"], true);
    Ok(())
}
