#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io::{ErrorKind, Read, Write};
#[cfg(target_os = "linux")]
use std::os::fd::AsFd;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;

use anyhow::{Context, Result, ensure};
#[cfg(target_os = "linux")]
use nix::errno::Errno;
#[cfg(target_os = "linux")]
use nix::fcntl::{OFlag, openat};
#[cfg(target_os = "linux")]
use nix::sys::stat::Mode;
#[cfg(target_os = "linux")]
use nix::unistd::geteuid;
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use tempfile::NamedTempFile;
use uuid::Uuid;

use crate::platform::StateDirectory;

const FILE_NAME: &str = "identity.json";
const SCHEMA_VERSION: u32 = 1;
const MAX_BYTES: u64 = 4096;

#[derive(Serialize, Deserialize)]
struct StoredIdentity {
    schema_version: u32,
    host_id: Uuid,
}

pub(crate) struct Identity {
    pub(crate) host_id: String,
    pub(crate) epoch: String,
}

impl Identity {
    pub(crate) fn load(state: &StateDirectory) -> Result<Self> {
        let host_id = match read(state)? {
            Some(host_id) => host_id,
            None => create(state)?,
        };
        Ok(Self {
            host_id: host_id.to_string(),
            epoch: Uuid::new_v4().to_string(),
        })
    }
}

#[cfg(target_os = "linux")]
fn read(state: &StateDirectory) -> Result<Option<Uuid>> {
    let fd = match openat(
        state,
        FILE_NAME,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::ENOENT) => return Ok(None),
        Err(error) => return Err(error).context("opening host identity"),
    };
    let file = File::from(fd);
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == geteuid().as_raw()
            && metadata.mode() & 0o777 == 0o600,
        "host identity must be a private regular file owned by this UID"
    );
    ensure!(
        metadata.len() <= MAX_BYTES,
        "host identity exceeds size limit"
    );

    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "host identity exceeds size limit"
    );
    decode(&bytes).map(Some)
}

fn decode(bytes: &[u8]) -> Result<Uuid> {
    let stored: StoredIdentity = serde_json::from_slice(bytes)
        .context("invalid host identity; repair identity.json before starting Corvid")?;
    ensure!(
        stored.schema_version == SCHEMA_VERSION,
        "unsupported host identity schema {}; use a compatible Corvid version",
        stored.schema_version
    );
    ensure!(!stored.host_id.is_nil(), "invalid nil host identity");
    Ok(stored.host_id)
}

#[cfg(target_os = "linux")]
fn create(state: &StateDirectory) -> Result<Uuid> {
    let host_id = Uuid::new_v4();
    let stored = StoredIdentity {
        schema_version: SCHEMA_VERSION,
        host_id,
    };
    let mut temporary = NamedTempFile::new_in(state.path())?;
    serde_json::to_writer(&mut temporary, &stored)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;

    match temporary.persist_noclobber(state.path().join(FILE_NAME)) {
        Ok(_) => {
            File::from(state.as_fd().try_clone_to_owned()?).sync_all()?;
            Ok(host_id)
        }
        Err(error) if error.error.kind() == ErrorKind::AlreadyExists => {
            read(state)?.context("host identity vanished during creation")
        }
        Err(error) => Err(error.error).context("publishing host identity"),
    }
}

#[cfg(windows)]
fn read(state: &StateDirectory) -> Result<Option<Uuid>> {
    state
        .read_private_file(FILE_NAME, MAX_BYTES)?
        .as_deref()
        .map(decode)
        .transpose()
}

#[cfg(windows)]
fn create(state: &StateDirectory) -> Result<Uuid> {
    let host_id = Uuid::new_v4();
    let stored = StoredIdentity {
        schema_version: SCHEMA_VERSION,
        host_id,
    };
    // The daemon holds the state lock throughout identity creation.
    state.replace_private_file(FILE_NAME, &serde_json::to_vec(&stored)?)?;
    Ok(host_id)
}
