use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use corvid_protocol::ProtocolFault;
use corvid_protocol::wire::{BrowseDirectories, DirectoryEntry, DirectoryListing};
use tokio::sync::Semaphore;
use tokio::time::timeout;

const MAX_PATH: usize = 4096;
const MAX_ENTRIES: usize = 128;
const MAX_SCAN: usize = 8192;
const MAX_RESULT_BYTES: usize = 48 * 1024;
static QUERIES: Semaphore = Semaphore::const_new(4);

pub(crate) async fn browse(query: BrowseDirectories) -> Result<DirectoryListing, ProtocolFault> {
    perform(move || listing(query)).await
}

pub(crate) async fn resolve(input: String) -> Result<Vec<u8>, ProtocolFault> {
    perform(move || resolve_path(&input).map(|path| path.into_os_string().into_encoded_bytes()))
        .await
}

async fn perform<T: Send + 'static>(
    action: impl FnOnce() -> Result<T, ProtocolFault> + Send + 'static,
) -> Result<T, ProtocolFault> {
    let permit = QUERIES
        .try_acquire()
        .map_err(|_| ProtocolFault::new("DirectoryBusy", "Folder lookups are busy. Try again."))?;
    timeout(
        Duration::from_secs(2),
        crate::executor::blocking(move || {
            let _permit = permit;
            action()
        }),
    )
    .await
    .map_err(|_| ProtocolFault::new("DirectoryTimeout", "Folder lookup timed out. Try again."))?
    .map_err(|_| ProtocolFault::new("DirectoryUnavailable", "Folder lookup failed."))?
}

fn resolve_path(input: &str) -> Result<PathBuf, ProtocolFault> {
    if input.is_empty() {
        return Err(invalid_path());
    }
    let path = absolute(input)?;
    let path = dunce::canonicalize(path).map_err(|error| inaccessible(&error))?;
    validate(&path)?;
    text_path(&path)?;
    Ok(path)
}

pub(crate) fn validate(path: &Path) -> Result<(), ProtocolFault> {
    if !fs::metadata(path)
        .map_err(|error| inaccessible(&error))?
        .is_dir()
    {
        return Err(ProtocolFault::new(
            "DirectoryNotFolder",
            "The selected path is not a folder on this host.",
        ));
    }
    #[cfg(unix)]
    nix::unistd::access(path, nix::unistd::AccessFlags::X_OK)
        .map_err(|error| inaccessible(&std::io::Error::from_raw_os_error(error as i32)))?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_TRAVERSE};
        fs::OpenOptions::new()
            .access_mode(FILE_TRAVERSE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .map_err(|error| inaccessible(&error))?;
    }
    Ok(())
}

fn home() -> Result<PathBuf, ProtocolFault> {
    std::env::home_dir()
        .filter(|path| path.is_absolute())
        .ok_or_else(|| ProtocolFault::new("DirectoryUnavailable", "The host has no home folder."))
}

fn absolute(input: &str) -> Result<PathBuf, ProtocolFault> {
    if input.len() > MAX_PATH || input.chars().any(char::is_control) {
        return Err(invalid_path());
    }
    let path = Path::new(input);
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    // Windows drive-relative paths depend on process state. Require a full drive path.
    if matches!(
        path.components().next(),
        Some(std::path::Component::Prefix(_))
    ) {
        return Err(invalid_path());
    }
    if let Ok(relative) = path.strip_prefix("~") {
        Ok(home()?.join(relative))
    } else {
        Ok(home()?.join(path))
    }
}

fn listing(query: BrowseDirectories) -> Result<DirectoryListing, ProtocolFault> {
    if query.validate_only {
        return description(&resolve_path(&query.path)?);
    }
    let path = absolute(&query.path)?;
    let separator = query.path.ends_with(std::path::is_separator);
    let (directory, prefix) = if query.complete
        && !separator
        && query.path != "~"
        && !query.path.is_empty()
        && let (Some(parent), Some(name)) = (path.parent(), path.file_name())
    {
        (parent, name.to_str().unwrap_or_default())
    } else {
        (path.as_path(), "")
    };
    let directory = dunce::canonicalize(directory).map_err(|error| inaccessible(&error))?;
    validate(&directory)?;
    let mut listing = description(&directory)?;
    let mut bytes = 0;
    let mut entries = fs::read_dir(&directory).map_err(|error| inaccessible(&error))?;
    for entry in entries.by_ref().take(MAX_SCAN) {
        let entry = entry.map_err(|error| inaccessible(&error))?;
        let name = entry.file_name();
        let Some(name) = name
            .to_str()
            .filter(|name| !name.chars().any(char::is_control))
        else {
            continue;
        };
        if !matches_prefix(name, prefix) {
            continue;
        }
        let kind = entry.file_type().map_err(|error| inaccessible(&error))?;
        if !query.show_hidden && !prefix.starts_with('.') && hidden(name, &entry) {
            continue;
        }
        let path = entry.path();
        if !(kind.is_dir() || kind.is_symlink() && path.is_dir()) {
            continue;
        }
        let Ok(path) = text_path(&path) else { continue };
        bytes += path.len() + name.len() + 16;
        if listing.entries.len() == MAX_ENTRIES || bytes > MAX_RESULT_BYTES {
            listing.truncated = true;
            break;
        }
        listing.entries.push(DirectoryEntry {
            name: name.to_owned(),
            path,
        });
    }
    listing.truncated |= entries.next().is_some();
    listing.entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(listing)
}

fn hidden(name: &str, _entry: &fs::DirEntry) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;
        if _entry
            .metadata()
            .is_ok_and(|metadata| metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0)
        {
            return true;
        }
    }
    name.starts_with('.')
}

fn description(directory: &Path) -> Result<DirectoryListing, ProtocolFault> {
    Ok(DirectoryListing {
        directory: text_path(directory)?,
        parent: directory.parent().and_then(|parent| text_path(parent).ok()),
        home: home().and_then(|home| text_path(&home)).unwrap_or_default(),
        folder_name: directory
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned),
        ..Default::default()
    })
}

fn matches_prefix(name: &str, prefix: &str) -> bool {
    if cfg!(windows) {
        name.to_lowercase().starts_with(&prefix.to_lowercase())
    } else {
        name.starts_with(prefix)
    }
}

fn text_path(path: &Path) -> Result<String, ProtocolFault> {
    path.to_str()
        .filter(|path| path.len() <= MAX_PATH && !path.chars().any(char::is_control))
        .map(str::to_owned)
        .ok_or_else(invalid_path)
}

fn invalid_path() -> ProtocolFault {
    ProtocolFault::new(
        "InvalidDirectory",
        "Enter a valid folder path on this host.",
    )
}

fn inaccessible(error: &std::io::Error) -> ProtocolFault {
    use std::io::ErrorKind;
    let (code, detail) = match error.kind() {
        ErrorKind::NotFound => (
            "DirectoryNotFound",
            "The folder no longer exists on this host.",
        ),
        ErrorKind::NotADirectory => (
            "DirectoryNotFolder",
            "The selected path is not a folder on this host.",
        ),
        ErrorKind::PermissionDenied => (
            "DirectoryDenied",
            "Access to the folder was denied on this host.",
        ),
        _ => (
            "DirectoryUnavailable",
            "The host could not open the folder.",
        ),
    };
    ProtocolFault::new(code, detail)
}
