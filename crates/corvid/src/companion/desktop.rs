use std::env;
use std::io;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_client::{ControlEndpoint, SshAlias};
use corvid_companion::{Config, HostEndpoint, PresentationClient};
use corvid_protocol::COMMAND_TIMEOUT;
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use super::DEFAULT_SLOTS;

const PIPE_POLL: Duration = Duration::from_millis(50);

pub(super) async fn open(instance: &str, state: &Path, alias: Option<String>) -> Result<()> {
    let board = env::current_exe()?.with_file_name("corvid-desktop.exe");
    ensure!(
        board.is_file(),
        "place corvid-desktop.exe beside corvid.exe, or use the Windows package"
    );

    let existing = connect(instance).await?;
    let config = match Config::read_if_present(state)? {
        Some(config) => config,
        None => {
            ensure!(
                existing.is_none(),
                "a companion is already running on this pipe; specify its --state-dir"
            );
            let alias = alias.as_deref().context(
                "first use requires an OpenSSH alias. Run corvid companion open work-host",
            )?;
            initialize(state, alias).await?
        }
    };
    let HostEndpoint::Ssh { alias: configured } = &config.endpoint;
    ensure!(
        alias
            .as_ref()
            .is_none_or(|requested| requested == configured),
        "this state directory is configured for SSH alias {configured}; use a separate state directory and pipe instance for another host"
    );

    let mut command = Command::new(board);
    command.args(["--pipe-instance", instance]);
    let interrupt = super::shutdown()?;
    if let Some(mut client) = existing {
        verify_companion(&mut client, &config).await?;
        drop(client);
        return tokio::select! {
            result = show_board(command) => result,
            _ = interrupt => Ok(()),
        };
    }

    let (stop, stopped) = oneshot::channel();
    let companion = corvid_companion::run(instance, state, async {
        let _ = stopped.await;
    });
    tokio::pin!(companion);
    let desktop = async {
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
        verify_companion(&mut client, &config).await?;
        drop(client);
        show_board(command).await
    };
    let result = tokio::select! {
        result = &mut companion => return result,
        result = desktop => result,
        _ = interrupt => Ok(()),
    };
    let _ = stop.send(());
    companion.await?;
    result
}

async fn show_board(mut command: Command) -> Result<()> {
    let mut board = command
        .kill_on_drop(true)
        .spawn()
        .context("opening Corvid Desktop")?;
    let status = board.wait().await.context("waiting for Corvid Desktop")?;
    ensure!(status.success(), "Corvid Desktop exited with {status}");
    Ok(())
}

async fn initialize(state: &Path, alias: &str) -> Result<Config> {
    let endpoint = ControlEndpoint::Ssh(SshAlias::new(alias.to_owned())?);
    let host = endpoint
        .host()
        .await
        .context("checking the Linux daemon through OpenSSH")?;
    corvid_companion::initialize(
        state,
        Uuid::parse_str(&host.host_id)?,
        HostEndpoint::Ssh {
            alias: alias.to_owned(),
        },
        DEFAULT_SLOTS,
    )?;
    Config::read_if_present(state)?.context("new companion configuration is missing")
}

async fn connect(instance: &str) -> Result<Option<PresentationClient>> {
    match PresentationClient::connect(instance).await {
        Ok(client) => Ok(Some(client)),
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error).context("checking the companion pipe"),
    }
}

async fn verify_companion(client: &mut PresentationClient, config: &Config) -> Result<()> {
    timeout(COMMAND_TIMEOUT, client.slots(Some(config)))
        .await
        .context("companion identity query timed out")??;
    Ok(())
}
