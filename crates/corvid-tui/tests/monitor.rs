use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_client::ControlEndpoint;
use corvid_companion::{Config, HostEndpoint, PresentationClient, monitor::Scope};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{CompanionConnection, StopAgentMonitor};
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

#[tokio::test]
async fn monitor_survives_readers_and_reuses_identity_without_an_agent_deck_process() -> Result<()>
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
    let executable = Path::new(env!("CARGO_BIN_EXE_corvid"));
    let result = async {
        let (first, second) = tokio::join!(
            scope.ensure_running(executable),
            scope.ensure_running(executable)
        );
        first?;
        second?;
        let mut ui = PresentationClient::connect(&scope.address).await?;
        let companion_epoch = ui.session().server_epoch.clone();
        timeout(Duration::from_secs(10), async {
            loop {
                let slots = ui.slots(Some(&scope.config)).await?;
                if slots.connection == CompanionConnection::Ready as i32 {
                    ensure!(
                        slots.remote_epoch.as_deref() == Some(host.server_epoch.as_str()),
                        "wrong daemon"
                    );
                    break Ok::<_, anyhow::Error>(());
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await??;
        drop(ui);
        scope.ensure_running(executable).await?;
        let mut hardware = PresentationClient::connect(&scope.address).await?;
        ensure!(
            hardware.session().server_epoch == companion_epoch,
            "monitor restarted with the display"
        );
        let slots = hardware.slots(Some(&scope.config)).await?;
        ensure!(
            slots.controller_id == controller.to_string(),
            "controller changed"
        );
        // A replacement verified route is consumed by the existing monitor, not by a UI.
        let absent = ControlEndpoint::Local(fixture.path().join("absent"));
        scope.configure(&absent)?;
        timeout(Duration::from_secs(10), async {
            loop {
                if hardware.slots(Some(&scope.config)).await?.connection
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
                if hardware.slots(Some(&scope.config)).await?.connection
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
