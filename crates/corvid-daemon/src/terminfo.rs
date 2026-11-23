use std::path::{Path, PathBuf};

use anyhow::Result;
use corvid_protocol::unix::RuntimeDir;
use portable_pty::CommandBuilder;

const PROFILE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/corvid.terminfo"));

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

pub(crate) fn configure(command: &mut CommandBuilder, runtime: &Path) {
    if command.get_env("TERMINFO").is_none() {
        command.env("TERMINFO", runtime.join("terminfo"));
    }
}
