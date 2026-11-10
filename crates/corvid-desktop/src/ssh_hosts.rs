use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use corvid_client::SshAlias;
use glob::{MatchOptions, Paths, Pattern, glob_with};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_FILES: usize = 64;
const MAX_DEPTH: usize = 16;
const MAX_ALIASES: usize = 512;

pub fn load() -> io::Result<Vec<SshAlias>> {
    let home = std::env::home_dir().ok_or(io::ErrorKind::NotFound)?;
    load_from(&home)
}

fn load_from(home: &Path) -> io::Result<Vec<SshAlias>> {
    let directory = home.join(".ssh");
    let mut scan = Scan {
        home,
        directory: &directory,
        files: HashSet::new(),
        names: HashSet::new(),
        aliases: Vec::new(),
    };
    scan.read(&directory.join("config"), 0)?;
    Ok(scan.aliases)
}

struct Scan<'a> {
    home: &'a Path,
    directory: &'a Path,
    files: HashSet<PathBuf>,
    names: HashSet<String>,
    aliases: Vec<SshAlias>,
}

impl Scan<'_> {
    fn read(&mut self, path: &Path, depth: usize) -> io::Result<()> {
        if self.files.len() >= MAX_FILES || depth >= MAX_DEPTH {
            return Ok(());
        }
        let path = match fs::canonicalize(path) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if !self.files.insert(path.clone()) || !fs::metadata(&path)?.is_file() {
            return Ok(());
        }
        let mut bytes = Vec::new();
        File::open(&path)?
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        let contents = String::from_utf8_lossy(&bytes);
        let mut unconditional = true;
        for line in contents.trim_start_matches('\u{feff}').lines() {
            let (keyword, value) = directive(line);
            if keyword.eq_ignore_ascii_case("Host") {
                let hosts = arguments(value).unwrap_or_default();
                unconditional = hosts.as_slice() == ["*"];
                self.add_hosts(hosts);
            } else if keyword.eq_ignore_ascii_case("Match") {
                unconditional = arguments(value).is_some_and(|args| {
                    matches!(args.as_slice(), [condition] if condition.eq_ignore_ascii_case("all"))
                });
            } else if unconditional && keyword.eq_ignore_ascii_case("Include") {
                for include in arguments(value).into_iter().flatten() {
                    if let Some(paths) = self.include_paths(&include) {
                        for path in paths.take(MAX_FILES).flatten() {
                            let _ = self.read(&path, depth + 1);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn add_hosts(&mut self, hosts: Vec<String>) {
        let excluded: Vec<_> = hosts
            .iter()
            .filter_map(|host| Pattern::new(host.strip_prefix('!')?).ok())
            .collect();
        for host in hosts {
            if self.aliases.len() >= MAX_ALIASES {
                return;
            }
            if excluded.iter().any(|pattern| pattern.matches(&host)) {
                continue;
            }
            if let Ok(alias) = SshAlias::new(host)
                && self.names.insert(alias.as_str().to_owned())
            {
                self.aliases.push(alias);
            }
        }
    }

    fn include_paths(&self, path: &str) -> Option<Paths> {
        if path.is_empty() || path.contains('%') || path.contains("${") {
            return None;
        }
        let pattern = if Path::new(path).is_absolute() {
            path.to_owned()
        } else {
            let (base, relative) = if let Some(relative) =
                path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"))
            {
                (self.home, relative)
            } else if path.starts_with('~') {
                return None;
            } else {
                (self.directory, path)
            };
            format!("{}/{relative}", Pattern::escape(base.to_str()?))
        };
        glob_with(
            &pattern,
            MatchOptions {
                case_sensitive: !cfg!(windows),
                require_literal_separator: true,
                require_literal_leading_dot: true,
            },
        )
        .ok()
    }
}

fn directive(line: &str) -> (&str, &str) {
    let line = line.trim_start();
    let index = line
        .find(|ch: char| ch.is_ascii_whitespace() || ch == '=')
        .unwrap_or(line.len());
    let (keyword, value) = line.split_at(index);
    let value = value.trim_start();
    (keyword, value.strip_prefix('=').unwrap_or(value))
}

fn arguments(value: &str) -> Option<Vec<String>> {
    let mut arguments = Vec::new();
    let mut chars = value.chars().peekable();
    loop {
        while chars.next_if(|ch| ch.is_ascii_whitespace()).is_some() {}
        if matches!(chars.peek(), None | Some('#')) {
            return Some(arguments);
        }

        let mut argument = String::new();
        let mut quote = None;
        while let Some(ch) = chars.next() {
            if ch == '\\'
                && let Some(next) =
                    chars.next_if(|next| matches!(next, '\\' | '\'' | '"' | ' ' | '\t'))
            {
                argument.push(next);
            } else if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() && matches!(ch, '\'' | '"') {
                quote = Some(ch);
            } else if quote.is_none() && ch.is_ascii_whitespace() {
                break;
            } else {
                argument.push(ch);
            }
        }
        if quote.is_some() {
            return None;
        }
        arguments.push(argument);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases(home: &Path) -> Vec<String> {
        load_from(home)
            .unwrap()
            .into_iter()
            .map(|alias| alias.as_str().to_owned())
            .collect()
    }

    #[test]
    fn lists_unique_concrete_host_aliases() {
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        fs::create_dir(&ssh).unwrap();
        fs::write(
            ssh.join("config"),
            concat!(
                "# Host comment\r\n",
                "  hOsT = build \"build-backup\" qa # trailing comment\r\n",
                "    HostName build.example.invalid\r\n",
                "Host qa * lab-? !blocked -option user@host host#tag\r\n",
                "Host stage-ok stage-skip !*-skip\r\n",
                "Host 'broken\r\n",
            ),
        )
        .unwrap();
        assert_eq!(
            aliases(home.path()),
            ["build", "build-backup", "qa", "stage-ok"]
        );
    }

    #[test]
    fn discovers_static_includes_without_following_cycles_or_conditions() {
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        fs::create_dir_all(ssh.join("profiles")).unwrap();
        fs::create_dir(ssh.join("shared configs")).unwrap();
        fs::write(
            ssh.join("config"),
            concat!(
                "Include profiles/*.conf\n",
                "Host local\n",
                "  Include conditional\n",
                "Match exec \"exit 1\"\n",
                "  Include conditional\n",
                "Host *\n",
                "  Include \"~/.ssh/shared configs/extra\"\n",
            ),
        )
        .unwrap();
        fs::write(ssh.join("profiles/a.conf"), "Include config\nHost alpha\n").unwrap();
        fs::write(ssh.join("profiles/b.conf"), "Host beta alpha\n").unwrap();
        fs::write(ssh.join("conditional"), "Host excluded\n").unwrap();
        fs::write(ssh.join("shared configs/extra"), "Host extra\n").unwrap();
        assert_eq!(aliases(home.path()), ["alpha", "beta", "local", "extra"]);
    }

    #[test]
    fn a_missing_user_config_keeps_manual_connect_available() {
        let home = tempfile::tempdir().unwrap();
        assert!(aliases(home.path()).is_empty());
    }
}
