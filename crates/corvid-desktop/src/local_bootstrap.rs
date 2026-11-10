use anyhow::{Context, Result};
use corvid_client::{ControlEndpoint, InteractiveConnection, daemon_missing, runtime_path};

pub async fn connect(
    capabilities: Vec<String>,
    progress: impl Fn(&str),
) -> Result<(ControlEndpoint, InteractiveConnection)> {
    let runtime = runtime_path(None)?;
    let endpoint = ControlEndpoint::Local(runtime.clone());
    match endpoint.interactive(capabilities.clone()).await {
        Ok(link) => return Ok((endpoint, link)),
        Err(error) if daemon_missing(&error) => {}
        Err(error) => return Err(error),
    }

    progress("Starting the local Corvid daemon…");
    corvid_tui::ensure_local_daemon(&runtime)
        .await
        .context("starting the local Corvid daemon")?;
    let link = endpoint.interactive(capabilities).await?;
    Ok((endpoint, link))
}
