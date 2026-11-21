use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_async::oneshot;
use corvid_async::time::{sleep, timeout};
use corvid_client::ControlEndpoint;
use corvid_companion::monitor::{CAPABILITY, Scope};
use corvid_companion::{
    Config, HostEndpoint, PRESENTATION_CAPABILITY, PresentationClient, RunKey, RunObservation,
    Store,
};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    CompanionConnection, FocusSlot, Pong, PresentationSlots, PresentedSlotToken, StopAgentMonitor,
};
use corvid_protocol::{FrameReader, FrameRole, FrameWriter, PeerRole, ProtocolFault, accept_hello};
use tokio::io::{AsyncRead, AsyncWrite};
use uuid::Uuid;

#[cfg(all(windows, feature = "compio-endpoints"))]
#[tokio::test]
async fn compio_companion_serves_while_detached_from_the_console() -> Result<()> {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};

    let fixture = tempfile::tempdir()?;
    let state = fixture.path().join("monitor");
    let host = Uuid::new_v4();
    let endpoint = ControlEndpoint::Local(fixture.path().join("absent"));
    let controller = corvid_companion::initialize(&state, host, HostEndpoint::from(&endpoint), 8)?;
    let address = controller.simple().to_string();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_corvid"))
        .args(["companion", "--state-dir"])
        .arg(&state)
        .args(["--pipe-instance", &address, "run"])
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;

    let ready = async {
        loop {
            if let Some(client) = PresentationClient::connect_if_present(&address).await? {
                return Ok::<_, anyhow::Error>(client);
            }
            sleep(Duration::from_millis(25)).await;
        }
    };
    let mut client = corvid_async::select! {
        result = timeout(Duration::from_secs(10), ready) => result.context("detached monitor never listened")??,
        exit = child.wait() => {
            let mut diagnostics = String::new();
            child.stderr.take().context("missing child stderr")?.read_to_string(&mut diagnostics).await?;
            anyhow::bail!("detached monitor exited {}: {}", exit?, diagnostics.trim());
        }
    };
    assert_eq!(client.session().host_id, host.to_string());
    assert_eq!(
        client.slots(None).await?.controller_id,
        controller.to_string()
    );
    assert!(matches!(
        client
            .request(Body::StopAgentMonitor(StopAgentMonitor {}))
            .await?,
        Body::Pong(_)
    ));
    assert!(
        timeout(Duration::from_secs(10), child.wait())
            .await??
            .success()
    );
    Ok(())
}

#[tokio::test]
async fn replaces_legacy_monitor_and_reconciles_saved_runs_without_restarting_daemon() -> Result<()>
{
    #[cfg(windows)]
    let fixture = tempfile::tempdir()?;
    #[cfg(not(windows))]
    let fixture = tempfile::tempdir_in(std::env::home_dir().context("home")?)?;
    let runtime = fixture.path().join("run");
    let daemon_runtime = runtime.clone();
    let daemon_state = fixture.path().join("daemon");
    let (stop, stopped) = oneshot::channel();
    let daemon = tokio::spawn(async move {
        let shutdown = async {
            let _ = stopped.await;
        };
        #[cfg(windows)]
        return corvid_daemon::run(&daemon_runtime, &daemon_state, shutdown).await;
        #[cfg(not(windows))]
        return corvid_daemon::run(&daemon_runtime, &daemon_state, false, shutdown).await;
    });
    let host = timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(host) = corvid_client::host(&runtime).await {
                return host;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await?;
    let endpoint = ControlEndpoint::Local(runtime.clone());
    let state = fixture.path().join("monitor");
    let controller = corvid_companion::initialize(
        &state,
        Uuid::parse_str(&host.host_id)?,
        HostEndpoint::from(&endpoint),
        8,
    )?;
    #[cfg(windows)]
    let address = controller.simple().to_string();
    #[cfg(not(windows))]
    let address = fixture.path().join("presentation");
    let scope = Scope {
        state: state.clone(),
        address,
        config: Config::read_if_present(&state)?.context("config")?,
    };
    scope.configure(&endpoint)?;
    let stale = RunKey {
        host: scope.config.host_id,
        epoch: Uuid::new_v4(),
        run: Uuid::new_v4(),
    };
    let mut saved = Store::open(&state)?;
    let revision = saved.update(|layout| {
        let run = RunObservation {
            created_sequence: Some(1),
            pane: None,
            eligible: true,
            navigable: true,
            ended: false,
        };
        layout.reconcile(&BTreeMap::from([(stale, run)]), &BTreeSet::new())?;
        Ok(layout.slots()[0].revision)
    })?;
    let legacy_epoch = Uuid::new_v4().to_string();
    let executable = Path::new(env!("CARGO_BIN_EXE_corvid"));
    let result = async {
        let (legacy, first, second) = corvid_async::join!(
            timeout(
                Duration::from_secs(10),
                legacy_monitor(&scope, &legacy_epoch, saved)
            ),
            scope.ensure_running(executable),
            scope.ensure_running(executable)
        );
        first?;
        second?;
        legacy??;
        let mut ui = PresentationClient::connect(&scope.address).await?;
        let companion_epoch = ui.session().server_epoch.clone();
        assert_ne!(companion_epoch, legacy_epoch);
        let synchronized = timeout(Duration::from_secs(10), async {
            loop {
                let slots = ui.slots(Some(&scope.config)).await?;
                if slots.connection == CompanionConnection::Ready as i32 {
                    ensure!(
                        slots.remote_epoch.as_deref() == Some(host.server_epoch.as_str()),
                        "wrong daemon"
                    );
                    break Ok::<_, anyhow::Error>(slots);
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await??;
        assert!(synchronized.slots.iter().all(|slot| slot.run.is_none()));
        assert_eq!(synchronized.recent_ended, [stale.into()]);
        let rejected = ui
            .request(Body::FocusSlot(FocusSlot {
                control_generation: synchronized.control_generation.context("generation")?,
                slot: Some(PresentedSlotToken {
                    slot_id: 1,
                    revision: Some(revision),
                    run: Some(stale.into()),
                }),
                binding: None,
            }))
            .await
            .expect_err("retired slot accepted focus");
        assert_eq!(
            rejected
                .downcast_ref::<ProtocolFault>()
                .map(|fault| fault.code.as_str()),
            Some("StaleBinding"),
        );
        drop(ui);
        scope.ensure_running(executable).await?;
        let mut observer = PresentationClient::connect(&scope.address).await?;
        ensure!(
            observer.session().server_epoch == companion_epoch,
            "monitor restarted with the display"
        );
        let slots = observer.slots(Some(&scope.config)).await?;
        ensure!(
            slots.controller_id == controller.to_string(),
            "controller changed"
        );
        // A replacement verified route is consumed by the existing monitor, not by a UI.
        let absent = ControlEndpoint::Local(fixture.path().join("absent"));
        scope.configure(&absent)?;
        timeout(Duration::from_secs(10), async {
            loop {
                if observer.slots(Some(&scope.config)).await?.connection
                    != CompanionConnection::Ready as i32
                {
                    break Ok::<_, anyhow::Error>(());
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await??;
        scope.configure(&endpoint)?;
        timeout(Duration::from_secs(10), async {
            loop {
                if observer.slots(Some(&scope.config)).await?.connection
                    == CompanionConnection::Ready as i32
                {
                    break Ok::<_, anyhow::Error>(());
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await??;
        ensure!(
            corvid_client::host(&runtime).await?.server_epoch == host.server_epoch,
            "execution daemon restarted"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Ok(mut monitor) = PresentationClient::connect(&scope.address).await {
        monitor
            .request(Body::StopAgentMonitor(StopAgentMonitor {}))
            .await?;
    }
    let _ = stop.send(());
    timeout(Duration::from_secs(10), daemon).await???;
    result
}

async fn legacy_monitor(scope: &Scope, epoch: &str, store: Store) -> Result<()> {
    #[cfg(windows)]
    {
        let mut listener = corvid_protocol::windows::PipeListener::bind(&scope.address)?;
        serve_legacy(listener.accept().await?, &scope.config, epoch).await?;
    }
    #[cfg(not(windows))]
    {
        use corvid_protocol::unix::{RuntimeDir, SocketRole};

        let runtime = RuntimeDir::create_all(&scope.address)?;
        let instance = runtime.lock_instance()?;
        let listener = instance.bind(SocketRole::Presentation).await?;
        serve_legacy(
            listener.accept().await?.context("legacy monitor peer")?,
            &scope.config,
            epoch,
        )
        .await?;
    }
    // Keep the save lock briefly after IPC closes, as the real service does when flushing.
    sleep(Duration::from_millis(100)).await;
    drop(store);
    Ok(())
}

async fn serve_legacy(
    stream: impl AsyncRead + AsyncWrite + Unpin,
    config: &Config,
    epoch: &str,
) -> Result<()> {
    let (input, output) = tokio::io::split(stream);
    let role = FrameRole::StructuredOnly;
    let mut writer = FrameWriter::start(output, role).await?;
    let mut reader = FrameReader::start(input, role).await?;
    let hello = reader.next_envelope().await?.context("legacy hello")?;
    let (session, reply) = accept_hello(
        &hello,
        PeerRole::Presentation,
        &config.host_id.to_string(),
        epoch,
        &[CAPABILITY, PRESENTATION_CAPABILITY],
    )?;
    writer.envelope(&reply).await?;

    let query = reader.next_envelope().await?.context("identity query")?;
    assert!(matches!(
        session.validate_request(&query)?,
        Body::GetPresentationSlots(_)
    ));
    let slots = PresentationSlots {
        controller_id: config.controller_id.to_string(),
        ..Default::default()
    };
    writer
        .envelope(&session.envelope(query.request_id, Body::PresentationSlots(slots)))
        .await?;

    let stop = reader.next_envelope().await?.context("shutdown request")?;
    assert!(matches!(
        session.validate_request(&stop)?,
        Body::StopAgentMonitor(_)
    ));
    writer
        .envelope(&session.envelope(stop.request_id, Body::Pong(Pong {})))
        .await?;
    Ok(())
}
