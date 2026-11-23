use std::path::PathBuf;

use anyhow::Result;
use corvid_protocol::unix::RuntimeDir;

const PROFILE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/corvid.terminfo"));

/// Select how the daemon supplies terminfo to child processes.
pub enum Mode {
    Inherit,
    Bundled,
}

/// Install the private PTY profile and return the directory used by TERMINFO.
pub fn install(runtime: &RuntimeDir) -> Result<PathBuf> {
    let database = runtime.path().join("terminfo");
    let buckets = if cfg!(target_os = "macos") {
        ["63", "78"]
    } else {
        ["c", "x"]
    };
    for (bucket, name) in buckets.into_iter().zip(["corvid", "xterm-corvid-256color"]) {
        let entries = RuntimeDir::create_all(&database.join(bucket))?;
        entries.replace_private_file(name, PROFILE)?;
    }
    Ok(database)
}
