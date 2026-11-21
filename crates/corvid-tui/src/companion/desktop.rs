use std::env;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use async_process::Command;

pub(super) async fn open(instance: &str, state: &Path, alias: Option<String>) -> Result<()> {
    let cli = env::current_exe()?;
    let desktop = corvid_update::layout::for_executable(&cli)?.desktop();
    let mut command = if desktop.is_file() {
        let mut command = Command::new(desktop);
        // Desktop opens a local terminal by default and accepts an explicit SSH alias.
        command.arg("--state-dir").arg(state);
        if let Some(alias) = alias {
            command.arg("--ssh-alias").arg(alias);
        }
        command
    } else {
        return corvid_agent_deck::launch::open(instance, state, alias.as_deref()).await;
    };
    command.args(["--pipe-instance", instance]);
    corvid_async::select! {
        result = show(command) => result,
        result = super::native_shutdown_signal() => result.map_err(Into::into),
    }
}

async fn show(mut command: Command) -> Result<()> {
    let mut app = command
        .kill_on_drop(true)
        .spawn()
        .context("opening Corvid Desktop")?;
    let status = app.status().await.context("waiting for Corvid Desktop")?;
    ensure!(status.success(), "Corvid Desktop exited with {status}");
    Ok(())
}
