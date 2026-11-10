use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_client::{ControlEndpoint, SshAlias};
use corvid_protocol::COMMAND_TIMEOUT;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use crate::{Config, HostEndpoint, PresentationClient};

pub const DEFAULT_SLOT_COUNT: u32 = 8;
const PIPE_POLL: Duration = Duration::from_millis(50);

pub enum DesktopStartup {
    NeedsHost,
    Reuse(Config),
    Start(Config),
}

pub fn default_state_dir() -> Result<PathBuf> {
    let base = env::var_os("LOCALAPPDATA")
        .filter(|path| !path.is_empty())
        .context("set LOCALAPPDATA or choose a companion state directory")?;
    Ok(PathBuf::from(base).join("Corvid").join("companion"))
}

pub async fn prepare_desktop(
    instance: &str,
    state: &Path,
    alias: Option<&str>,
) -> Result<DesktopStartup> {
    let existing = connect(instance).await?;
    let config = match Config::read_if_present(state)? {
        Some(config) => config,
        None => {
            ensure!(
                existing.is_none(),
                "a companion already owns this pipe; choose its state directory"
            );
            let Some(alias) = alias else {
                return Ok(DesktopStartup::NeedsHost);
            };
            initialize(state, alias).await?
        }
    };
    if let Some(requested) = alias {
        let HostEndpoint::Ssh { alias: configured } = &config.endpoint else {
            anyhow::bail!("this state directory is configured for a local daemon");
        };
        ensure!(
            requested == configured,
            "this state directory is configured for SSH alias {configured}"
        );
    }
    if let Some(mut existing) = existing {
        verify(&mut existing, &config).await?;
        return Ok(DesktopStartup::Reuse(config));
    }
    Ok(DesktopStartup::Start(config))
}

pub async fn wait_for_desktop(instance: &str, config: &Config) -> Result<()> {
    let mut client = timeout(COMMAND_TIMEOUT, async {
        loop {
            if let Some(client) = connect(instance).await? {
                return Ok::<_, anyhow::Error>(client);
            }
            sleep(PIPE_POLL).await;
        }
    })
    .await
    .context("companion pipe did not start")??;
    verify(&mut client, config).await
}

async fn initialize(state: &Path, alias: &str) -> Result<Config> {
    let endpoint = ControlEndpoint::Ssh(SshAlias::new(alias.to_owned())?);
    let host = endpoint
        .host()
        .await
        .context("checking the Linux daemon through OpenSSH")?;
    crate::initialize(
        state,
        Uuid::parse_str(&host.host_id)?,
        HostEndpoint::Ssh {
            alias: alias.to_owned(),
        },
        DEFAULT_SLOT_COUNT,
    )?;
    Config::read_if_present(state)?.context("new companion configuration is missing")
}

async fn connect(instance: &str) -> Result<Option<PresentationClient>> {
    PresentationClient::connect_if_present(instance)
        .await
        .context("checking the companion pipe")
}

async fn verify(client: &mut PresentationClient, config: &Config) -> Result<()> {
    timeout(COMMAND_TIMEOUT, client.slots(Some(config)))
        .await
        .context("companion identity query timed out")??;
    Ok(())
}
