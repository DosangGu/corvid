use std::env;
use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result};

pub fn runtime_path(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) =
        override_path.or_else(|| env::var_os("CORVID_RUNTIME_DIR").map(PathBuf::from))
    {
        return Ok(path);
    }

    #[cfg(target_os = "linux")]
    let (variable, suffix, hint) = (
        "XDG_RUNTIME_DIR",
        "corvid",
        "set XDG_RUNTIME_DIR or --runtime-dir",
    );
    #[cfg(windows)]
    let (variable, suffix, hint) = (
        "LOCALAPPDATA",
        "Corvid/runtime",
        "LOCALAPPDATA is unavailable",
    );

    let base = env::var_os(variable)
        .filter(|value| !value.is_empty())
        .context(hint)?;
    Ok(PathBuf::from(base).join(suffix))
}

pub fn daemon_missing(error: &anyhow::Error) -> bool {
    #[cfg(target_os = "linux")]
    if error.downcast_ref::<nix::errno::Errno>() == Some(&nix::errno::Errno::ENOENT) {
        return true;
    }

    error.downcast_ref::<io::Error>().is_some_and(|error| {
        error.kind() == io::ErrorKind::NotFound
            || (cfg!(target_os = "linux") && error.kind() == io::ErrorKind::ConnectionRefused)
    })
}
