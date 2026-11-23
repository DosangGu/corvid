//! Start and replace daemon processes independently of the presentation clients.

use std::path::{Path, PathBuf};

use anyhow::Result;

pub mod identity;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod unix;
#[cfg(windows)]
pub mod windows;

pub fn state_path(override_path: Option<PathBuf>) -> Result<PathBuf> {
    corvid_protocol::paths::state_directory(
        corvid_protocol::paths::StateRole::Daemon,
        override_path,
    )
}

async fn verify_installed_version(executable: &Path, previous_version: &str) -> Result<()> {
    use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault};

    let output = corvid_async::time::timeout(
        COMMAND_TIMEOUT,
        async_process::Command::new(executable)
            .arg("--version")
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    let response = std::str::from_utf8(&output.stdout)?.trim_end_matches(['\r', '\n']);
    let version = response
        .strip_prefix("corvid ")
        .or_else(|| response.strip_prefix("corvidd "));
    anyhow::ensure!(
        output.status.success() && version == Some(previous_version),
        ProtocolFault::new(
            "InstallationChanged",
            "the installed Corvid version changed; reconnect and retry"
        )
    );
    Ok(())
}
