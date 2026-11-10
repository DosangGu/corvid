use std::env;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use corvid_companion::launch::{DesktopStartup, prepare_desktop, wait_for_desktop};
use tokio::process::Command;
use tokio::sync::oneshot;

pub(super) async fn open(instance: &str, state: &Path, alias: Option<String>) -> Result<()> {
    let cli = env::current_exe()?;
    let desktop = cli.with_file_name("corvid-desktop.exe");
    let mut command = if desktop.is_file() {
        let mut command = Command::new(desktop);
        // The GPUI app manages startup itself, including its first-run form.
        command.arg("--state-dir").arg(state);
        if let Some(alias) = alias {
            command.arg("--ssh-alias").arg(alias);
        }
        command
    } else {
        let board = cli.with_file_name("corvid-slot-board.exe");
        ensure!(
            board.is_file(),
            "place corvid-desktop.exe beside corvid.exe, or use the Windows package"
        );
        let mut command = Command::new(board);
        command.args(["--pipe-instance", instance]);
        return open_board(command, instance, state, alias.as_deref()).await;
    };
    command.args(["--pipe-instance", instance]);
    tokio::select! {
        result = show(command) => result,
        _ = super::shutdown()? => Ok(()),
    }
}

async fn open_board(
    command: Command,
    instance: &str,
    state: &Path,
    alias: Option<&str>,
) -> Result<()> {
    let config = match prepare_desktop(instance, state, alias).await? {
        DesktopStartup::NeedsHost => {
            bail!("first use requires an OpenSSH alias. Run corvid companion open work-host")
        }
        DesktopStartup::Reuse(_) => {
            return tokio::select! {
                result = show(command) => result,
                _ = super::shutdown()? => Ok(()),
            };
        }
        DesktopStartup::Start(config) => config,
    };
    let (stop, stopped) = oneshot::channel();
    let companion = corvid_companion::run(instance, state, async {
        let _ = stopped.await;
    });
    tokio::pin!(companion);
    let desktop = async {
        wait_for_desktop(instance, &config).await?;
        show(command).await
    };
    let result = tokio::select! {
        result = &mut companion => return result,
        result = desktop => result,
        _ = super::shutdown()? => Ok(()),
    };
    let _ = stop.send(());
    companion.await?;
    result
}

async fn show(mut command: Command) -> Result<()> {
    let mut app = command
        .kill_on_drop(true)
        .spawn()
        .context("opening Corvid Desktop")?;
    let status = app.wait().await.context("waiting for Corvid Desktop")?;
    ensure!(status.success(), "Corvid Desktop exited with {status}");
    Ok(())
}
