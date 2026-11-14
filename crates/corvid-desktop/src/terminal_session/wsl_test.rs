use super::*;
use corvid_client::{DaemonInstallation, HostPlatform};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

async fn wait_for(session: &Session, ready: impl Fn(&Ui) -> bool) -> Result<Ui> {
    let mut changes = session.changes();
    timeout(Duration::from_secs(20), async {
        loop {
            let ui = session.ui();
            if ready(&ui) {
                return Ok(ui);
            }
            ensure!(ui.changing || ui.connected, "{}", ui.message);
            changes.changed().await?;
        }
    })
    .await
    .context("waiting for the WSL session")?
}

async fn wait_screen(session: &Session, pane: &str, marker: &str) -> Result<()> {
    let mut changes = session.screen_changes();
    timeout(Duration::from_secs(15), async {
        loop {
            if session.screen(pane, |screen| screen.contents().contains(marker)) == Some(true) {
                return Ok(());
            }
            changes.changed().await?;
        }
    })
    .await
    .context("waiting for WSL terminal output")?
}

async fn install_prompt(
    session: &Session,
    prompts: &mut mpsc::UnboundedReceiver<Prompt>,
) -> Result<Prompt> {
    let prompt = timeout(Duration::from_secs(20), prompts.recv()).await?;
    let Some(prompt) = prompt else {
        let ui = wait_for(session, |ui| !ui.changing).await?;
        bail!("missing install confirmation: {}", ui.message);
    };
    ensure!(
        prompt.action == Some("Install"),
        "unexpected setup operation"
    );
    Ok(prompt)
}

// Run in a disposable WSL profile with a matching daemon bundle beside the test
// executable. CORVID_RUNTIME_DIR and XDG_STATE_HOME must be forwarded via WSLENV.
// The fixture owner removes the Linux test installation and stops its daemon.
#[tokio::test]
#[ignore = "requires an isolated WSL profile and a matching Linux daemon bundle"]
async fn wsl_session_installs_and_reconnects_to_the_same_shell() -> Result<()> {
    let runtime = std::env::var("CORVID_RUNTIME_DIR")?;
    ensure!(
        runtime.contains("/corvid-wsl-test-"),
        "use a disposable WSL runtime"
    );
    let distro = WslDistribution::new(std::env::var("CORVID_WSL_TEST_DISTRIBUTION")?)?;
    ensure!(WslDistribution::list().await?.contains(&distro));
    let remote = RemoteHost::Wsl(distro.clone());
    ensure!(remote.probe_platform().await? == HostPlatform::Linux);
    ensure!(
        remote.probe_daemon(HostPlatform::Linux).await?.is_none(),
        "use a profile without Corvid installed"
    );
    let state = tempfile::tempdir()?;
    let start = || {
        Session::start(
            Destination::Wsl(distro.clone()),
            state.path().join("desktop"),
            ViewStore::new(state.path().join("desktop/desktop-view")),
            AttachTarget::Default,
            true,
        )
    };

    let (session, job, mut prompts) = start()?;
    install_prompt(&session, &mut prompts)
        .await?
        .decision
        .send(false)
        .unwrap();
    job.join();
    assert!(session.ui().message.contains("canceled"));
    ensure!(remote.probe_daemon(HostPlatform::Linux).await?.is_none());

    let (session, mut job, mut prompts) = start()?;
    let result = verify_live_session(&session, &mut prompts, distro).await;
    job.stop();
    job.join();
    result
}

async fn verify_live_session(
    session: &Session,
    prompts: &mut mpsc::UnboundedReceiver<Prompt>,
    distro: WslDistribution,
) -> Result<()> {
    install_prompt(session, prompts)
        .await?
        .decision
        .send(true)
        .unwrap();
    let initial = wait_for(session, |ui| {
        ui.connected && ui.inventory_ready && ui.terminal_id.is_some()
    })
    .await?;
    let selected = initial.selected.clone().context("missing selected pane")?;
    let pane = &selected.pane_id;

    let mut frames = session.screen_changes();
    timeout(Duration::from_secs(10), async {
        while session.screen(pane, |_| ()).is_none() {
            frames.changed().await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let input = b"CORVID_WSL_PID=$$; CORVID_WSL_VALUE=retained; printf 'WSL_%s\\n' READY\n";
    ensure!(
        session.send(pane, input.to_vec(), false),
        "input was rejected"
    );
    wait_screen(session, pane, "WSL_READY").await?;

    let endpoint = ControlEndpoint::Wsl(distro.clone());
    let before = endpoint.host().await?;
    let output = timeout(
        Duration::from_secs(20),
        tokio::process::Command::new(
            PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/wsl.exe"),
        )
        .creation_flags(CREATE_NO_WINDOW)
        .args([
            "--distribution",
            distro.as_str(),
            "--exec",
            "/bin/sh",
            "-c",
            "exec \"$HOME/.corvid/corvid\" upgrade",
        ])
        .kill_on_drop(true)
        .output(),
    )
    .await??;
    ensure!(output.status.success(), "WSL daemon upgrade failed");
    let after = endpoint.host().await?;
    assert_eq!(before.host_id, after.host_id);
    assert_ne!(before.server_epoch, after.server_epoch);
    wait_for_reconnect(session, &endpoint, &initial).await?;
    wait_screen(session, pane, "WSL_READY").await?;
    assert_eq!(session.ui().terminal_id, initial.terminal_id);

    let input =
        b"test \"$CORVID_WSL_PID\" = \"$$\" && printf 'WSL_%s_%s\\n' AFTER \"$CORVID_WSL_VALUE\"\n";
    ensure!(
        session.send(pane, input.to_vec(), false),
        "input was rejected"
    );
    wait_screen(session, pane, "WSL_AFTER_retained").await?;
    session.create(Target::TabLabel("WSL test tab".into()));
    let tabs = wait_for(session, |ui| {
        !ui.changing
            && ui
                .selected
                .as_ref()
                .is_some_and(|current| current.tab_id != selected.tab_id)
    })
    .await?;
    session.create(Target::SplitAxis(
        corvid_protocol::wire::SplitAxis::Vertical as i32,
    ));
    wait_for(session, |ui| !ui.changing && ui.selected != tabs.selected).await?;
    session.select(selected.clone(), false);
    wait_for(session, |ui| {
        !ui.changing && ui.selected.as_ref() == Some(&selected)
    })
    .await?;
    wait_screen(session, pane, "WSL_AFTER_retained").await?;

    let version = RemoteHost::Wsl(distro)
        .daemon_version(HostPlatform::Linux, DaemonInstallation::Managed)
        .await?;
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
    println!(
        "Verified WSL discovery, install cancellation, approved install, terminal input, screen, daemon replacement, retained shell, tabs, and split panes."
    );
    Ok(())
}

async fn wait_for_reconnect(
    session: &Session,
    endpoint: &ControlEndpoint,
    initial: &Ui,
) -> Result<()> {
    let host = initial
        .host_id
        .as_deref()
        .context("missing host identity")?;
    timeout(Duration::from_secs(20), async {
        loop {
            let ui = session.ui();
            if ui.connected && ui.inventory_ready {
                let watch = endpoint.watch(&Uuid::new_v4().to_string(), host, || {}).await?;
                let attached = watch.state().snapshot().entities().any(|entity| {
                    matches!(
                        &entity.body,
                        Some(EntityBody::Client(client)) if client.active_terminal() == initial.terminal_id.as_deref()
                    )
                });
                if attached {
                    return Ok::<_, anyhow::Error>(());
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("Desktop did not reconnect through WSL")?
}
