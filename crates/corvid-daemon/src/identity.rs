#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::fs::File;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::{ErrorKind, Write};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::fd::AsFd;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use anyhow::Context;
use anyhow::Result;
use corvid_client::daemon_control::identity::{FILE_NAME, encode, read};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use tempfile::NamedTempFile;
use uuid::Uuid;

use crate::platform::StateDirectory;

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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn create(state: &StateDirectory) -> Result<Uuid> {
    let host_id = Uuid::new_v4();
    let mut temporary = NamedTempFile::new_in(state.path())?;
    temporary.write_all(&encode(host_id)?)?;
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
fn create(state: &StateDirectory) -> Result<Uuid> {
    let host_id = Uuid::new_v4();
    // The daemon holds the state lock throughout identity creation.
    state.replace_private_file(FILE_NAME, &encode(host_id)?)?;
    Ok(host_id)
}
