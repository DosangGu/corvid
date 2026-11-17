use std::io;
use std::path::PathBuf;

use anyhow::Result;
use corvid_protocol::paths;

pub fn runtime_path(override_path: Option<PathBuf>) -> Result<PathBuf> {
    paths::daemon_runtime(override_path, || None)?.prepare()
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
