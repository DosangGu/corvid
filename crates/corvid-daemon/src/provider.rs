mod profile;

use std::env;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use corvid_core::agent::Provider;
use corvid_protocol::collector::{CLAUDE_VERSION, CODEX_VERSION};
use nix::fcntl::{FcntlArg, OFlag, fcntl};

use crate::process::Executable;

pub(crate) use profile::adapter;

const VERSION_TIMEOUT: Duration = Duration::from_millis(350);

pub(crate) struct QualifiedProvider {
    pub(crate) path: PathBuf,
    pub(crate) executable: Executable,
}

impl QualifiedProvider {
    pub(crate) fn detect(provider: Provider) -> Option<Self> {
        let (program, expected) = match provider {
            Provider::Claude => ("claude", format!("{CLAUDE_VERSION} (Claude Code)")),
            Provider::Codex => ("codex", format!("codex-cli {CODEX_VERSION}")),
        };
        let path = find_on_path(program)?;
        let executable = Executable::read(&path)?;
        let version = version_output(&path)?;
        if version.trim() != expected || Executable::read(&path) != Some(executable) {
            return None;
        }
        Some(Self { path, executable })
    }
}

pub(crate) fn qualified_version(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => CLAUDE_VERSION,
        Provider::Codex => CODEX_VERSION,
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths).find_map(|directory| {
        let path = directory.join(name);
        let metadata = path.metadata().ok()?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
        path.canonicalize().ok()
    })
}

fn version_output(path: &Path) -> Option<String> {
    let mut child = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + VERSION_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let output = child.stdout.take()?;
    let flags = OFlag::from_bits_retain(fcntl(&output, FcntlArg::F_GETFL).ok()?);
    fcntl(&output, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).ok()?;
    let mut version = String::new();
    output.take(128).read_to_string(&mut version).ok()?;
    Some(version)
}
