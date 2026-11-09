use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::Result;
use corvid_core::agent::Provider;
use corvid_protocol::unix::RuntimeDir;
use portable_pty::CommandBuilder;

use crate::process::Executable;

use super::{find_on_path, observer_arguments, program, qualify};

pub(crate) struct Commands(PathBuf);

impl Commands {
    pub(crate) fn install(runtime: &Path) -> Result<Self> {
        let directory = RuntimeDir::create_all(&runtime.join("agent-bin"))?;
        let executable = env::current_exe()?.canonicalize()?;
        let staging = tempfile::tempdir_in(directory.path())?;
        for provider in [Provider::Claude, Provider::Codex] {
            let name = program(provider);
            let link = staging.path().join(name);
            symlink(&executable, &link)?;
            fs::rename(link, directory.path().join(name))?;
        }
        directory.replace_private_file(".zshenv", include_bytes!("shell.zshenv"))?;
        Ok(Self(directory.path().to_owned()))
    }

    pub(crate) fn configure(&self, command: &mut CommandBuilder) -> bool {
        let path = command
            .get_env("PATH")
            .unwrap_or(OsStr::new("/usr/bin:/bin"));
        let paths = std::iter::once(self.0.clone()).chain(env::split_paths(path));
        let Ok(path) = env::join_paths(paths) else {
            return false;
        };
        command.env("PATH", path);
        command.env("CORVID_SHELL_COMMANDS", &self.0);
        // A login profile can prepend its own bin directories to PATH.
        let shell = command.get_shell();
        match Path::new(&shell).file_name().and_then(OsStr::to_str) {
            Some("bash") => configure_bash(command),
            Some("zsh") => self.configure_zsh(command),
            _ => {}
        }
        true
    }

    fn configure_zsh(&self, command: &mut CommandBuilder) {
        if let Some(original) = command.get_env("ZDOTDIR").map(OsStr::to_owned) {
            command.env("CORVID_ORIGINAL_ZDOTDIR", original);
        } else {
            command.env_remove("CORVID_ORIGINAL_ZDOTDIR");
        }
        command.env("ZDOTDIR", &self.0);
    }
}

fn configure_bash(command: &mut CommandBuilder) {
    for provider in [Provider::Claude, Provider::Codex] {
        let name = program(provider);
        let key = format!("BASH_FUNC_{name}%%");
        if command.get_env(&key).is_some() {
            continue;
        }
        let body = format!(
            r#"() {{
                local wrapper="$CORVID_SHELL_COMMANDS/{name}"
                if [ -x "$wrapper" ]; then
                    command "$wrapper" "$@"
                else
                    command {name} "$@"
                fi
            }}"#
        );
        command.env(key, body);
    }
}

pub fn invoked_provider(argv0: &OsStr) -> Option<Provider> {
    match Path::new(argv0).file_name()?.to_str()? {
        "claude" => Some(Provider::Claude),
        "codex" => Some(Provider::Codex),
        _ => None,
    }
}

pub fn resolve(provider: Provider) -> Option<PathBuf> {
    let helper = Executable::read(Path::new("/proc/self/exe"))?;
    find_on_path(program(provider), Some(helper))
}

pub fn qualified_arguments(
    provider: Provider,
    executable: &Path,
    argv: &[OsString],
) -> Option<Vec<OsString>> {
    qualify(provider, executable)?;
    Some(observer_arguments(provider, argv))
}
