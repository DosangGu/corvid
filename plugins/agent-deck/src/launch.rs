use std::path::Path;

use anyhow::{Context, Result, bail};
use corvid_companion::launch::{DesktopStartup, prepare_desktop};
use corvid_companion::monitor::Scope;

/// The CLI delegates Agents startup here so executable discovery and process
/// lifetime remain owned by the Agent Deck package.
pub async fn open(instance: &str, state: &Path, alias: Option<&str>) -> Result<()> {
    let config = match prepare_desktop(instance, state, alias).await? {
        DesktopStartup::NeedsHost => {
            bail!("first use requires an OpenSSH alias: corvid companion open work-host")
        }
        DesktopStartup::Reuse(config) | DesktopStartup::Start(config) => config,
    };
    let scope = crate::scope::shared_scope(Scope {
        state: state.to_owned(),
        address: instance.to_owned(),
        config,
    })
    .await?;
    let mut client = crate::lifecycle::ensure_current(
        &crate::desktop::executable()?,
        &scope.state,
        &scope.address,
        &scope.config,
        true,
    )
    .await?
    .context("Agent Deck was not started")?;
    client.activate(false).await
}
