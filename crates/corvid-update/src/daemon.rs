//! Locate and install the standalone daemon for the client release.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use corvid_protocol::unix::RuntimeDir as StateDirectory;
#[cfg(windows)]
use corvid_protocol::windows::StateDirectory;

use crate::{Product, RELEASES, download, layout, version};

const MAX_DAEMON_BYTES: u64 = 32 * 1024 * 1024;

pub const EXECUTABLE: &str = if cfg!(windows) {
    "corvidd.exe"
} else {
    "corvidd"
};

/// Resolve bundled and managed daemons without making a network request.
pub fn find(client: &Path, state: &Path, client_version: &str) -> Result<Option<PathBuf>> {
    if let Some(bundled) = layout::bundled_daemon(client)? {
        return Ok(Some(bundled));
    }

    let directory = install_directory(state, client_version)?;
    match fs::symlink_metadata(&directory) {
        Ok(_) => {
            let _private = StateDirectory::open(&directory)?;
            installed(&directory)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("locating the installed Corvid daemon"),
    }
}

/// Install the matching release daemon into a private, versioned directory.
/// Callers obtain user consent before invoking this function.
pub fn install(state: &Path, client_version: &str) -> Result<PathBuf> {
    let directory = install_directory(state, client_version)?;
    let _private = StateDirectory::create_all(&directory)?;
    if let Some(binary) = installed(&directory)? {
        return Ok(binary);
    }

    let release =
        crate::check(Some(client_version)).context("finding the matching Corvid daemon release")?;
    let target = crate::target(Product::Tui)?;
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let name = format!("corvidd-{}-{target}{suffix}", release.version);
    let asset = release.asset(&name)?;
    ensure!(
        asset.size <= MAX_DAEMON_BYTES,
        "Corvid daemon exceeds the allowed size"
    );

    let temporary = tempfile::NamedTempFile::new_in(&directory)?;
    download::file(
        &format!("{RELEASES}/download/v{}/{name}", release.version),
        temporary.path(),
        asset.size,
    )?;
    download::verify_file(temporary.path(), asset.size, &asset.sha256)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    temporary.as_file().sync_all()?;

    let path = directory.join(EXECUTABLE);
    match temporary.persist_noclobber(&path) {
        Ok(_) => Ok(path),
        Err(error) if error.error.kind() == ErrorKind::AlreadyExists => {
            let binary =
                installed(&directory)?.context("concurrent daemon installation disappeared")?;
            download::verify_file(&binary, asset.size, &asset.sha256)?;
            Ok(binary)
        }
        Err(error) => Err(error.error).context("publishing the Corvid daemon"),
    }
}

fn install_directory(state: &Path, client_version: &str) -> Result<PathBuf> {
    Ok(state
        .join("daemon-bin")
        .join(version(client_version)?.to_string())
        .join(crate::target(Product::Tui)?))
}

fn installed(directory: &Path) -> Result<Option<PathBuf>> {
    let path = directory.join(EXECUTABLE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "installed daemon must be a regular file"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                ensure!(
                    metadata.permissions().mode() & 0o777 == 0o700,
                    "installed daemon must be private and executable"
                );
            }
            Ok(Some(path))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("checking the installed Corvid daemon"),
    }
}
