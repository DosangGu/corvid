//! Read the daemon's persisted host identity before a managed replacement.

use std::path::Path;

use anyhow::{Context, Result, ensure};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use corvid_protocol::unix::RuntimeDir as StateDirectory;
#[cfg(windows)]
use corvid_protocol::windows::StateDirectory;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const FILE_NAME: &str = "identity.json";
const SCHEMA_VERSION: u32 = 1;
const MAX_BYTES: u64 = 4096;

#[derive(Serialize, Deserialize)]
struct StoredIdentity {
    schema_version: u32,
    host_id: Uuid,
}

pub fn stored_host_id(state: &Path) -> Result<Option<Uuid>> {
    read(&StateDirectory::open(state)?)
}

pub fn read(state: &StateDirectory) -> Result<Option<Uuid>> {
    let Some(bytes) = state.read_private_file(FILE_NAME, MAX_BYTES)? else {
        return Ok(None);
    };
    let stored: StoredIdentity = serde_json::from_slice(&bytes)
        .context("invalid host identity; repair identity.json before starting Corvid")?;
    ensure!(
        stored.schema_version == SCHEMA_VERSION,
        "unsupported host identity schema {}; use a compatible Corvid version",
        stored.schema_version
    );
    ensure!(!stored.host_id.is_nil(), "invalid nil host identity");
    Ok(Some(stored.host_id))
}

pub fn encode(host_id: Uuid) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&StoredIdentity {
        schema_version: SCHEMA_VERSION,
        host_id,
    })?)
}
