use anyhow::{Context, Result, ensure};
use corvid_protocol::PeerRole;
use tokio::process::Command;

use super::{DaemonInstallation, HostPlatform, bootstrap};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WslDistribution {
    name: String,
    pub(super) daemon: Option<DaemonInstallation>,
}

impl WslDistribution {
    pub fn new(name: String) -> Result<Self> {
        ensure!(
            !name.is_empty()
                && name.len() <= 256
                && !name.starts_with('-')
                && name.trim() == name
                && !name.chars().any(char::is_control),
            "invalid WSL distribution name"
        );
        Ok(Self { name, daemon: None })
    }

    pub fn as_str(&self) -> &str {
        &self.name
    }

    pub(super) fn command(&self, role: PeerRole) -> Result<Command> {
        let arguments = match role {
            PeerRole::Control => "control --stdio",
            PeerRole::Tui => "terminal-relay",
            _ => unreachable!("WSL only exposes the two daemon client roles"),
        };
        let remote = match self.daemon {
            Some(daemon) => bootstrap::daemon_command(HostPlatform::Linux, daemon, arguments),
            None => bootstrap::linux_command(&format!("auto {arguments}")),
        };
        self.remote_command(&remote)
    }

    pub(super) fn remote_command(&self, remote: &str) -> Result<Command> {
        let mut command = wsl_command()?;
        command
            .args([
                "--distribution",
                self.as_str(),
                "--cd",
                "~",
                "--exec",
                "/bin/sh",
                "-c",
                remote,
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        Ok(command)
    }

    pub async fn list() -> Result<Vec<Self>> {
        use tokio::io::AsyncReadExt;

        let mut child = wsl_command()?
            .args(["--list", "--quiet"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("listing WSL distributions")?;
        let mut stdout = child
            .stdout
            .take()
            .context("WSL stdout is unavailable")?
            .take(65537);
        let mut bytes = Vec::new();
        let (status, read) = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            tokio::join!(child.wait(), stdout.read_to_end(&mut bytes))
        })
        .await
        .context("listing WSL distributions timed out")?;
        ensure!(
            status?.success(),
            "Could not list WSL distributions. Check that WSL is installed."
        );
        read?;
        ensure!(bytes.len() <= 65536, "WSL distribution list is too large");
        parse_distributions(&bytes)
    }
}

#[cfg(windows)]
fn wsl_command() -> Result<Command> {
    let mut command = Command::new(super::system_directory()?.join("wsl.exe"));
    command
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
        .kill_on_drop(true);
    Ok(command)
}

#[cfg(not(windows))]
fn wsl_command() -> Result<Command> {
    anyhow::bail!("WSL connections require Windows")
}

fn parse_distributions(bytes: &[u8]) -> Result<Vec<WslDistribution>> {
    let text = if bytes.starts_with(&[0xff, 0xfe]) || bytes.contains(&0) {
        let bytes = bytes.strip_prefix(&[0xff, 0xfe]).unwrap_or(bytes);
        ensure!(
            bytes.len().is_multiple_of(2),
            "invalid WSL distribution list encoding"
        );
        let units: Vec<_> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16(&units).context("invalid WSL distribution list encoding")?
    } else {
        std::str::from_utf8(bytes)?
            .trim_start_matches('\u{feff}')
            .to_owned()
    };
    let mut distributions = Vec::new();
    for name in text.lines().filter(|name| !name.trim().is_empty()) {
        let distribution = WslDistribution::new(name.to_owned())?;
        if !distributions.contains(&distribution) {
            distributions.push(distribution);
        }
    }
    Ok(distributions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_utf16_and_utf8_distribution_names() {
        let names = "Ubuntu\r\n개발 Linux\r\nUbuntu\r\n";
        let wide: Vec<_> = names.encode_utf16().flat_map(u16::to_le_bytes).collect();
        for bytes in [
            names.as_bytes().to_vec(),
            wide.clone(),
            [vec![0xff, 0xfe], wide].concat(),
        ] {
            let names: Vec<_> = parse_distributions(&bytes)
                .unwrap()
                .into_iter()
                .map(|d| d.name)
                .collect();
            assert_eq!(names, ["Ubuntu", "개발 Linux"]);
        }
        assert!(parse_distributions(&[b'U', 0, b'b']).is_err());
    }

    #[test]
    fn rejects_options_and_control_characters() {
        for invalid in ["", "--exec", "Ubuntu\n", "Ubuntu\0", " Ubuntu"] {
            assert!(WslDistribution::new(invalid.into()).is_err());
        }
    }

    #[cfg(windows)]
    #[test]
    fn passes_distribution_names_as_single_arguments() {
        let distro = WslDistribution::new("개발 Linux; echo unexpected".into()).unwrap();
        let command = distro.remote_command("printf ready").unwrap();
        let args: Vec<_> = command.as_std().get_args().collect();
        assert_eq!(
            args,
            [
                "--distribution",
                distro.as_str(),
                "--cd",
                "~",
                "--exec",
                "/bin/sh",
                "-c",
                "printf ready"
            ]
        );
    }
}
