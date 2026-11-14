use std::env;
use std::io;
use std::path::PathBuf;

#[cfg(not(target_os = "macos"))]
use anyhow::Context;
use anyhow::Result;

pub fn runtime_path(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) =
        override_path.or_else(|| env::var_os("CORVID_RUNTIME_DIR").map(PathBuf::from))
    {
        return Ok(path);
    }

    #[cfg(target_os = "macos")]
    {
        corvid_protocol::macos::runtime_directory()
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

    #[cfg(not(target_os = "macos"))]
    let base = env::var_os(variable)
        .filter(|value| !value.is_empty())
        .context(hint)?;
    #[cfg(not(target_os = "macos"))]
    Ok(PathBuf::from(base).join(suffix))
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
