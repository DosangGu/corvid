#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_async::time::{sleep, timeout};
use corvid_companion::{Config, PresentationClient};
use corvid_protocol::wire::{CompanionConnection, HostInfo};
use tokio::process::{Child, Command};

const WAIT: Duration = Duration::from_secs(10);

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_corvid"));
    command
        .env("HOME", home)
        .env("XDG_RUNTIME_DIR", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env_remove("CORVID_RUNTIME_DIR")
        .kill_on_drop(true);
    command
}

async fn start_daemon(home: &Path, runtime: &Path) -> Result<(Child, HostInfo)> {
    let mut daemon = command(home).arg("daemon").spawn()?;
    let host = timeout(WAIT, async {
        loop {
            ensure!(daemon.try_wait()?.is_none(), "daemon exited before binding");
            if let Ok(host) = corvid_client::host(runtime).await {
                return Ok::<_, anyhow::Error>(host);
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    Ok((daemon, host))
}

#[tokio::test]
async fn daemon_and_companion_restart_using_existing_default_locations() -> Result<()> {
    let fixture = tempfile::tempdir_in(std::env::home_dir().context("home directory")?)?;
    let home = fixture.path();
    #[cfg(target_os = "linux")]
    let (runtime, state, presentation) = (
        home.join("corvid"),
        home.join("state/corvid"),
        home.join("corvid-companion"),
    );
    #[cfg(target_os = "macos")]
    let (runtime, state, presentation) = {
        let boot = corvid_protocol::macos::boot_session()?;
        let runtime = home.join(".corvid").join(format!("r-{}", &boot[..16]));
        (
            runtime.clone(),
            home.join("Library/Application Support/Corvid"),
            runtime.join("corvid-companion"),
        )
    };

    let (mut daemon, first) = start_daemon(home, &runtime).await?;
    let mut init = command(home);
    init.args(["companion", "init", "--host-id", &first.host_id]);
    let result = timeout(WAIT, init.output()).await??;
    ensure!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let config =
        Config::read_if_present(&state.join("companion"))?.context("missing companion state")?;
    let mut host = first;
    for restart in [false, true] {
        if restart {
            daemon.kill().await?;
            let (child, next) = start_daemon(home, &runtime).await?;
            assert_eq!(host.host_id, next.host_id);
            assert_ne!(host.server_epoch, next.server_epoch);
            daemon = child;
            host = next;
        }

        let mut companion = command(home).args(["companion", "run"]).spawn()?;
        timeout(WAIT, async {
            loop {
                ensure!(
                    companion.try_wait()?.is_none(),
                    "companion exited before connecting"
                );
                if let Some(mut client) =
                    PresentationClient::connect_if_present(&presentation).await?
                {
                    let slots = client.slots(Some(&config)).await?;
                    if slots.connection == CompanionConnection::Ready as i32 {
                        assert_eq!(slots.controller_id, config.controller_id.to_string());
                        assert_eq!(
                            slots.remote_epoch.as_deref(),
                            Some(host.server_epoch.as_str())
                        );
                        return Ok::<(), anyhow::Error>(());
                    }
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        let stopped = timeout(WAIT, command(home).args(["companion", "stop"]).status()).await??;
        assert!(stopped.success());
        assert!(timeout(WAIT, companion.wait()).await??.success());
    }
    daemon.kill().await?;
    Ok(())
}
