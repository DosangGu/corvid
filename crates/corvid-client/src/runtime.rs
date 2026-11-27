use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use corvid_protocol::ProtocolFault;
use corvid_protocol::paths::{self, DaemonRuntime};

const REMEMBERED_RUNTIME_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

pub fn runtime_path(override_path: Option<PathBuf>) -> Result<PathBuf> {
    paths::daemon_runtime(override_path)?.prepare()
}

pub async fn tui_runtime(override_path: Option<PathBuf>) -> Result<PathBuf> {
    resolve_tui_runtime_with(
        paths::daemon_runtime(override_path),
        crate::view_state::last_tui_runtime,
        remembered_runtime_reachable,
    )
    .await
}

async fn resolve_tui_runtime_with<F, Fut>(
    resolved: Result<DaemonRuntime>,
    saved: impl FnOnce() -> Option<PathBuf>,
    is_reachable: F,
) -> Result<PathBuf>
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: Future<Output = bool>,
{
    match resolved {
        Ok(DaemonRuntime::Override(path)) => path.prepare(),
        Ok(DaemonRuntime::Default(default)) => match saved() {
            Some(remembered) if remembered != default.path() => {
                if is_reachable(remembered.clone()).await {
                    Ok(remembered)
                } else {
                    default.prepare()
                }
            }
            _ => default.prepare(),
        },
        Err(error) => {
            if let Some(remembered) = saved()
                && is_reachable(remembered.clone()).await
            {
                return Ok(remembered);
            }
            Err(error)
        }
    }
}

async fn remembered_runtime_reachable(path: PathBuf) -> bool {
    match corvid_async::time::timeout(REMEMBERED_RUNTIME_PROBE_TIMEOUT, crate::ping(&path)).await {
        Ok(Ok(())) => true,
        Ok(Err(error)) => error
            .downcast_ref::<ProtocolFault>()
            .is_some_and(|fault| fault.code == "ProtocolMismatch"),
        Err(_) => false,
    }
}

pub fn daemon_missing(error: &anyhow::Error) -> bool {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if error.downcast_ref::<nix::errno::Errno>() == Some(&nix::errno::Errno::ENOENT) {
        return true;
    }

    error.downcast_ref::<io::Error>().is_some_and(|error| {
        error.kind() == io::ErrorKind::NotFound
            || (cfg!(any(target_os = "linux", target_os = "macos"))
                && error.kind() == io::ErrorKind::ConnectionRefused)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use corvid_protocol::paths::RuntimePath;

    fn override_runtime(path: &str) -> Result<DaemonRuntime> {
        Ok(DaemonRuntime::Override(RuntimePath::direct(PathBuf::from(
            path,
        ))))
    }

    fn default_runtime(path: &str) -> Result<DaemonRuntime> {
        Ok(DaemonRuntime::Default(RuntimePath::direct(PathBuf::from(
            path,
        ))))
    }

    #[compio::test]
    async fn override_bypasses_saved_runtime_and_reachability_probe() {
        let resolved = resolve_tui_runtime_with(
            override_runtime("/override"),
            || panic!("override must not consult saved view"),
            |_| async { panic!("override must not probe reachability") },
        )
        .await
        .unwrap();
        assert_eq!(resolved, PathBuf::from("/override"));
    }

    #[compio::test]
    async fn default_without_saved_runtime_uses_default_without_probing() {
        let resolved = resolve_tui_runtime_with(
            default_runtime("/default"),
            || None,
            |_| async { panic!("missing saved runtime must not probe reachability") },
        )
        .await
        .unwrap();
        assert_eq!(resolved, PathBuf::from("/default"));
    }

    #[compio::test]
    async fn default_matching_saved_runtime_uses_fast_path_without_probing() {
        let resolved = resolve_tui_runtime_with(
            default_runtime("/default"),
            || Some(PathBuf::from("/default")),
            |_| async { panic!("matching default runtime must not probe reachability") },
        )
        .await
        .unwrap();
        assert_eq!(resolved, PathBuf::from("/default"));
    }

    #[compio::test]
    async fn reachable_saved_runtime_is_preserved() {
        let mut probed = None;
        let resolved = resolve_tui_runtime_with(
            default_runtime("/default"),
            || Some(PathBuf::from("/remembered")),
            |path| {
                probed = Some(path);
                async { true }
            },
        )
        .await
        .unwrap();
        assert_eq!(probed, Some(PathBuf::from("/remembered")));
        assert_eq!(resolved, PathBuf::from("/remembered"));
    }

    #[compio::test]
    async fn unreachable_saved_runtime_falls_back_to_default() {
        let mut probed = None;
        let resolved = resolve_tui_runtime_with(
            default_runtime("/default"),
            || Some(PathBuf::from("/stale")),
            |path| {
                probed = Some(path);
                async { false }
            },
        )
        .await
        .unwrap();
        assert_eq!(probed, Some(PathBuf::from("/stale")));
        assert_eq!(resolved, PathBuf::from("/default"));
    }

    #[compio::test]
    async fn default_resolution_error_uses_reachable_saved_or_returns_error() {
        let reachable = resolve_tui_runtime_with(
            Err(anyhow!("missing XDG_RUNTIME_DIR")),
            || Some(PathBuf::from("/remembered")),
            |_| async { true },
        )
        .await
        .unwrap();
        assert_eq!(reachable, PathBuf::from("/remembered"));

        let err = resolve_tui_runtime_with(
            Err(anyhow!("missing XDG_RUNTIME_DIR")),
            || Some(PathBuf::from("/stale")),
            |_| async { false },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("missing XDG_RUNTIME_DIR"));
    }
}
