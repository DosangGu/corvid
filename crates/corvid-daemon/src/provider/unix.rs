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
use corvid_protocol::wire::ProviderProfile;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use semver::Version;

use super::{CONTRACTS, adapter, contract, hooks};
use crate::process::Executable;

const VERSION_TIMEOUT: Duration = Duration::from_millis(1500);
const VERSION_OUTPUT_LIMIT: usize = 128;
const LOGIN_PATH_TIMEOUT: Duration = Duration::from_millis(500);
const LOGIN_PATH_LIMIT: usize = 16 * 1024;

pub(crate) fn qualify(provider: Provider, path: &Path) -> Option<(Executable, Version)> {
    let executable = Executable::read(path)?;
    let version = contract(provider).version(&version_output(path)?)?;
    (Executable::read(path) == Some(executable)).then_some((executable, version))
}

pub(crate) fn program(provider: Provider) -> &'static str {
    contract(provider).unix.program
}

pub(crate) fn installed_program(provider: Provider) -> Option<PathBuf> {
    find_on_path(program(provider), None)
}

pub(crate) fn observer_arguments(
    provider: Provider,
    version: &Version,
    argv: &[OsString],
) -> Vec<OsString> {
    contract(provider)
        .unix
        .launch_prefix
        .iter()
        .map(OsString::from)
        .chain(hooks::arguments(provider, version, argv))
        .collect()
}

pub(crate) fn qualified_version(provider: Provider) -> &'static str {
    contract(provider).unix.fixture_version
}

pub(crate) fn qualified_profiles() -> &'static [ProviderProfile] {
    static PROFILES: OnceLock<Vec<ProviderProfile>> = OnceLock::new();
    PROFILES.get_or_init(|| {
        CONTRACTS
            .iter()
            .map(|contract| ProviderProfile {
                provider: contract.wire_id,
                version: contract.unix.fixture_version.to_owned(),
                adapter: Some(adapter(contract.provider, true)),
            })
            .collect()
    })
}

pub(crate) fn find_on_path(name: &str, skip: Option<Executable>) -> Option<PathBuf> {
    let matches_executable = |directory: PathBuf| {
        let path = directory.join(name);
        let metadata = path.metadata().ok()?;
        if !metadata.is_file()
            || metadata.permissions().mode() & 0o111 == 0
            || skip.is_some_and(|skip| Executable::read(&path) == Some(skip))
        {
            return None;
        }
        std::path::absolute(path).ok()
    };
    let env_path = env::var_os("PATH");
    if let Some(paths) = env_path.as_deref()
        && let Some(found) = env::split_paths(paths).find_map(&matches_executable)
    {
        return Some(found);
    }
    if let Some(home) = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        let user_dirs = [
            home.join(".local/bin"),
            home.join(".cargo/bin"),
            home.join(".bun/bin"),
        ];
        if let Some(found) = user_dirs.into_iter().find_map(&matches_executable) {
            return Some(found);
        }
    }
    if env_path.as_deref().is_some_and(|paths| !paths.is_empty()) {
        if let Some(found) = ["/opt/homebrew/bin", "/usr/local/bin"]
            .into_iter()
            .map(PathBuf::from)
            .find_map(&matches_executable)
        {
            return Some(found);
        }
        if let Some(paths) = login_shell_path()
            && let Some(found) = env::split_paths(paths).find_map(&matches_executable)
        {
            return Some(found);
        }
    }
    None
}

fn login_shell_path() -> Option<&'static OsString> {
    static LOGIN_PATH: OnceLock<Option<OsString>> = OnceLock::new();
    LOGIN_PATH
        .get_or_init(|| {
            let shell = env::var_os("SHELL").filter(|value| !value.is_empty())?;
            let mut child = Command::new(shell)
                .args(["-lc", "printf '%s' \"$PATH\""])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let deadline = Instant::now() + LOGIN_PATH_TIMEOUT;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) if status.success() => break,
                    Ok(Some(_)) => return None,
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                }
            }
            let mut bytes = Vec::new();
            child
                .stdout?
                .take(LOGIN_PATH_LIMIT as u64 + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.is_empty() || bytes.len() > LOGIN_PATH_LIMIT || bytes.contains(&0) {
                return None;
            }
            let text = String::from_utf8(bytes).ok()?;
            Some(OsString::from(text))
        })
        .as_ref()
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
        for (provider, banner) in [
            (Provider::Claude, "1.2.3 (Claude Code)\n"),
            (Provider::Codex, "codex-cli 1.2.3\n"),
            (Provider::Antigravity, "1.5.17\n"),
        ] {
            let (reader, mut writer) = io::pipe()?;
            writer.write_all(banner.as_bytes())?;

            let version = read_version_output(reader).expect("complete version banner was lost");
            assert_eq!(version, banner);
            assert!(contract(provider).version(&version).is_some());
            drop(writer);
        }
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
