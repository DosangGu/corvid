use anyhow::Result;
use corvid_client::{ControlEndpoint, InteractiveConnection, daemon_missing, runtime_path};

use crate::daemon_prompt::Operation;
use crate::daemon_prompt::{Prompts, needs_upgrade};

pub async fn connect(
    capabilities: Vec<String>,
    explicit: bool,
    prompts: &Prompts,
    progress: impl Fn(&str),
) -> Result<(ControlEndpoint, InteractiveConnection)> {
    let runtime = runtime_path(None)?;
    let endpoint = ControlEndpoint::Local(runtime.clone());
    let connection = match endpoint.interactive(capabilities.clone()).await {
        Err(error) if explicit && daemon_missing(&error) => {
            progress("Starting the local Corvid daemon…");
            match corvid_tui::ensure_local_daemon(&runtime).await {
                Ok(()) => endpoint.interactive(capabilities.clone()).await,
                Err(error) => Err(error.context("starting the local Corvid daemon")),
            }
        }
        connection => connection,
    };
    let link = match connection {
        Err(error) if explicit && needs_upgrade(&error) => {
            upgrade(&runtime, prompts, &progress).await?;
            endpoint.interactive(capabilities).await?
        }
        connection => connection?,
    };
    Ok((endpoint, link))
}

async fn upgrade(
    runtime: &std::path::Path,
    prompts: &Prompts,
    progress: impl Fn(&str),
) -> Result<()> {
    prompts
        .confirm(
            Operation::Upgrade,
            "Local",
            "Replace the local Corvid daemon.\n\nHosted shells keep running.\nClients reconnect after the upgrade.",
        )
        .await?;
    progress("Upgrading the local Corvid daemon…");
    corvid_tui::upgrade_local_daemon(runtime).await
}
