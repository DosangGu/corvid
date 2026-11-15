pub(crate) mod hooks;
mod profile;
pub mod shell;

use std::env;
use std::ffi::OsString;
use std::io::{ErrorKind, Read};
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use corvid_core::agent::Provider;
use corvid_protocol::collector::{CLAUDE_VERSION, CODEX_VERSION};
use corvid_protocol::wire::ProviderProfile;
use nix::fcntl::{FcntlArg, OFlag, fcntl};

use crate::process::Executable;
use crate::projection;

pub(crate) use profile::adapter;

const VERSION_TIMEOUT: Duration = Duration::from_millis(350);
const VERSION_OUTPUT_LIMIT: usize = 128;

pub(crate) fn qualify(provider: Provider, path: &Path) -> Option<Executable> {
    let expected = match provider {
        Provider::Claude => format!("{CLAUDE_VERSION} (Claude Code)"),
        Provider::Codex => format!("codex-cli {CODEX_VERSION}"),
    };
    let executable = Executable::read(path)?;
    let version = version_output(path)?;
    if version.trim() != expected || Executable::read(path) != Some(executable) {
        return None;
    }
    Some(executable)
}

pub(crate) fn program(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    }
}

pub(crate) fn installed_program(provider: Provider) -> Option<PathBuf> {
    find_on_path(program(provider), None)?.canonicalize().ok()
}

pub(crate) fn observer_arguments(provider: Provider, argv: &[OsString]) -> Vec<OsString> {
    let mut arguments = Vec::new();
    if provider == Provider::Codex {
        // A shared Codex app server can outlive the managed launcher.
        arguments.extend([
            "-c".into(),
            OsString::from("features.daemon_auto_start=false"),
        ]);
    }
    arguments.extend(hooks::arguments(provider, argv));
    arguments
}

pub(crate) fn qualified_version(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => CLAUDE_VERSION,
        Provider::Codex => CODEX_VERSION,
    }
}

pub(crate) fn qualified_profiles() -> &'static [ProviderProfile] {
    static PROFILES: OnceLock<[ProviderProfile; 2]> = OnceLock::new();
    PROFILES.get_or_init(|| {
        [Provider::Claude, Provider::Codex].map(|provider| ProviderProfile {
            provider: projection::provider(provider) as i32,
            version: qualified_version(provider).to_owned(),
            adapter: Some(adapter(provider, true)),
        })
    })
}

pub(crate) fn find_on_path(name: &str, skip: Option<Executable>) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths).find_map(|directory| {
        let path = directory.join(name);
        let metadata = path.metadata().ok()?;
        if !metadata.is_file()
            || metadata.permissions().mode() & 0o111 == 0
            || skip.is_some_and(|skip| Executable::read(&path) == Some(skip))
        {
            return None;
        }
        std::path::absolute(path).ok()
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
    read_version_output(child.stdout.take()?)
}

fn read_version_output(output: impl Read + AsFd) -> Option<String> {
    let flags = OFlag::from_bits_retain(fcntl(&output, FcntlArg::F_GETFL).ok()?);
    fcntl(&output, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).ok()?;
    let mut bytes = Vec::new();
    let read = output
        .take(VERSION_OUTPUT_LIMIT as u64 + 1)
        .read_to_end(&mut bytes);
    match read {
        Ok(_) => {}
        // A concurrent fork can keep a copy of the write end until exec.
        // The probe has exited, so a complete line need not wait for pipe EOF.
        Err(error) if error.kind() == ErrorKind::WouldBlock && bytes.ends_with(b"\n") => {}
        Err(_) => return None,
    }
    if bytes.len() > VERSION_OUTPUT_LIMIT {
        return None;
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};

    #[test]
    fn reads_complete_version_while_another_process_keeps_stdout_open() -> io::Result<()> {
        let (reader, mut writer) = io::pipe()?;
        writer.write_all(b"codex-cli 1.2.3\n")?;

        assert_eq!(
            read_version_output(reader).as_deref(),
            Some("codex-cli 1.2.3\n")
        );
        Ok(())
    }

    #[test]
    fn requires_a_complete_line_when_the_stdout_pipe_is_still_open() -> io::Result<()> {
        let (reader, mut writer) = io::pipe()?;
        writer.write_all(b"codex-cli 1.2.3")?;

        assert_eq!(read_version_output(reader), None);
        Ok(())
    }

    #[test]
    fn rejects_oversized_version_output() -> io::Result<()> {
        let (reader, mut writer) = io::pipe()?;
        writer.write_all(&[b' '; VERSION_OUTPUT_LIMIT + 1])?;
        drop(writer);

        assert_eq!(read_version_output(reader), None);
        Ok(())
    }
}
