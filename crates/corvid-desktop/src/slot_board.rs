use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_client::SshAlias;
use corvid_companion::{Config, HostEndpoint, PresentationAddress, PresentationClient};
use corvid_protocol::COMMAND_TIMEOUT;
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

const DEFAULT_SLOT_COUNT: u32 = 8;

#[derive(Clone)]
pub(crate) struct Settings {
    pub state: PathBuf,
    #[cfg(windows)]
    pub pipe_instance: String,
}

struct Scope {
    address: <PresentationAddress as ToOwned>::Owned,
    state: PathBuf,
}

pub(crate) async fn open(settings: Settings, alias: SshAlias, host: Uuid) -> Result<()> {
    let executable = std::env::current_exe()?
        .parent()
        .context("locating the Corvid Desktop installation")?
        .join("plugins/slot-board")
        .join(format!("corvid-slot-board{}", std::env::consts::EXE_SUFFIX));
    ensure!(
        executable.is_file(),
        "Slot Board is unavailable. Install its plugin beside Corvid Desktop."
    );

    let (scope, config) = settings.scope(alias.as_str(), host)?;
    if let Some(client) = PresentationClient::connect_if_present(&scope.address).await? {
        verify(client, &config).await?;
        return show(&executable, &scope, &config).await;
    }

    let (stop, stopped) = oneshot::channel();
    let companion =
        corvid_companion::run_for_ssh(&scope.address, &scope.state, alias, host, async {
            let _ = stopped.await;
        });
    let window = async {
        let client = wait_for_companion(&scope.address).await?;
        verify(client, &config).await?;
        show(&executable, &scope, &config).await
    };
    tokio::pin!(companion, window);
    let result = tokio::select! {
        result = &mut companion => return result,
        result = &mut window => result,
    };
    let _ = stop.send(());
    companion.await?;
    result
}

impl Settings {
    fn scope(&self, alias: &str, host: Uuid) -> Result<(Scope, Config)> {
        let shared = Config::read_if_present(&self.state)?.is_some_and(|config| {
            config.host_id == host
                && matches!(config.endpoint, HostEndpoint::Ssh { alias: configured } if configured == alias)
        });
        let state = if shared {
            self.state.clone()
        } else {
            self.state
                .join("slot-board")
                .join(host.simple().to_string())
        };
        let config = prepare_state(&state, alias, host)?;
        #[cfg(windows)]
        let address = if shared {
            self.pipe_instance.clone()
        } else {
            config.controller_id.simple().to_string()
        };
        #[cfg(target_os = "linux")]
        let address = {
            let runtime = std::env::var_os("XDG_RUNTIME_DIR")
                .filter(|path| !path.is_empty())
                .context("set XDG_RUNTIME_DIR to open Slot Board")?;
            let directory = if shared {
                "corvid-companion".to_owned()
            } else {
                format!("cv-slot-{}", config.controller_id.simple())
            };
            PathBuf::from(runtime).join(directory)
        };
        Ok((Scope { address, state }, config))
    }
}

fn prepare_state(state: &Path, alias: &str, host: Uuid) -> Result<Config> {
    let config = match Config::read_if_present(state)? {
        Some(config) => config,
        None => {
            let created = corvid_companion::initialize(
                state,
                host,
                HostEndpoint::Ssh {
                    alias: alias.into(),
                },
                DEFAULT_SLOT_COUNT,
            );
            match Config::read_if_present(state)? {
                Some(config) => config,
                None => {
                    created?;
                    anyhow::bail!("Slot Board companion configuration is missing");
                }
            }
        }
    };
    ensure!(
        config.host_id == host && matches!(config.endpoint, HostEndpoint::Ssh { .. }),
        "Slot Board companion configuration belongs to a different host"
    );
    Ok(config)
}

async fn wait_for_companion(address: &PresentationAddress) -> Result<PresentationClient> {
    timeout(COMMAND_TIMEOUT, async {
        loop {
            if let Some(client) = PresentationClient::connect_if_present(address).await? {
                return Ok::<_, anyhow::Error>(client);
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("Slot Board companion did not start")?
}

async fn verify(mut client: PresentationClient, config: &Config) -> Result<()> {
    timeout(COMMAND_TIMEOUT, client.slots(Some(config)))
        .await
        .context("Slot Board companion identity query timed out")??;
    Ok(())
}

async fn show(executable: &Path, scope: &Scope, config: &Config) -> Result<()> {
    let mut command = Command::new(executable);
    command
        .arg("--host-id")
        .arg(config.host_id.to_string())
        .arg("--controller-id")
        .arg(config.controller_id.to_string());
    #[cfg(windows)]
    command.arg("--pipe-instance").arg(&scope.address);
    #[cfg(target_os = "linux")]
    command.arg("--socket-dir").arg(&scope.address);
    let status = command
        .kill_on_drop(true)
        .spawn()
        .context("opening Slot Board")?
        .wait()
        .await
        .context("waiting for Slot Board")?;
    ensure!(status.success(), "Slot Board exited with {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_the_controller_and_refuses_a_different_host() -> Result<()> {
        #[cfg(target_os = "linux")]
        let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
        #[cfg(windows)]
        let directory = tempfile::tempdir()?;
        let state = directory.path().join("slot-board");
        let host = Uuid::new_v4();
        let first = prepare_state(&state, "work-host", host)?;
        let reopened = prepare_state(&state, "work-host", host)?;
        assert_eq!(first.controller_id, reopened.controller_id);
        assert!(prepare_state(&state, "other-host", Uuid::new_v4()).is_err());
        assert_eq!(Config::read_if_present(&state)?.unwrap().host_id, host);
        Ok(())
    }
}
