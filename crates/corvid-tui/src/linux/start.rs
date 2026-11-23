use std::path::{Path, PathBuf};

use anyhow::Result;
use corvid_client::AttachOptions;
use corvid_client::daemon_control::unix;
use corvid_protocol::ProtocolFault;
use uuid::Uuid;

pub(crate) async fn start(override_path: Option<PathBuf>) -> Result<u32> {
    corvid_client::check_tty()?;
    let path = corvid_client::view_state::tui_runtime(override_path)?;
    crate::daemon_bootstrap::ensure_cli(&path).await?;

    let options = AttachOptions {
        remember_view: true,
        launch_if_empty: true,
        ..AttachOptions::default()
    };
    let result = crate::attach_monitored(&path, options).await;
    let fault = result
        .as_ref()
        .err()
        .and_then(|error| error.downcast_ref::<ProtocolFault>());
    if fault.is_some_and(|fault| fault.code == "NoTerminal")
        && corvid_client::view_state::tui_store()
            .and_then(|store| store.for_target(&corvid_client::view_state::tui_target(&path).ok()?))
            .is_none()
    {
        // Older daemons only support a separate, explicit shell launch.
        let options = AttachOptions {
            remember_view: true,
            launch: Some(Vec::new()),
            ..AttachOptions::default()
        };
        return crate::attach_monitored(&path, options).await;
    }
    result
}

pub(crate) async fn ensure(path: &Path) -> Result<()> {
    unix::ensure(path, None, |_| crate::local_daemon_executable()).await
}

pub(crate) async fn upgrade(path: &Path) -> Result<()> {
    unix::upgrade(path, |_| crate::local_daemon_executable()).await
}

pub(crate) async fn desktop_handoff(path: &Path, host: Uuid, epoch: Uuid) -> Result<()> {
    unix::desktop_handoff(path, host, epoch, |_| crate::local_daemon_executable()).await
}

pub(super) use unix::upgrade_managed;
