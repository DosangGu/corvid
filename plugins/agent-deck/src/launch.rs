use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use corvid_companion::launch::{DesktopStartup, prepare_desktop};
use corvid_protocol::COMMAND_TIMEOUT;
use tokio::process::Command;
use tokio::time::{sleep, timeout};

/// The CLI delegates plugin startup here so executable discovery and plugin
/// lifetime remain owned by the Agent Deck package.
pub async fn open(instance: &str, state: &Path, alias: Option<&str>) -> Result<()> {
    let config = match prepare_desktop(instance, state, alias).await? {
        DesktopStartup::NeedsHost => {
            bail!("first use requires an OpenSSH alias: corvid companion open work-host")
        }
        DesktopStartup::Reuse(config) | DesktopStartup::Start(config) => config,
    };
    if crate::connection::Client::connect(config.host_id, config.controller_id, false)
        .await
        .is_ok()
    {
        return Ok(());
    }
    let mut child = Command::new(crate::desktop::executable()?)
        .arg("--state-dir")
        .arg(state)
        .arg("--pipe-instance")
        .arg(instance)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x0800_0000)
        .spawn()
        .context("starting Corvid Agent Deck")?;
    timeout(COMMAND_TIMEOUT, async {
        loop {
            if crate::connection::Client::connect(config.host_id, config.controller_id, false)
                .await
                .is_ok()
            {
                return Ok(());
            }
            if let Some(status) = child.try_wait()? {
                bail!("Corvid Agent Deck exited during startup ({status})");
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("Agent Deck startup timed out")?
}
