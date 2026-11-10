use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_protocol::ProtocolFault;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use super::{SshAlias, drain_diagnostic, ssh_failure};

const SCRIPT: &str = include_str!("bootstrap.sh");
const MAX_RESPONSE: usize = 64;
const MAX_BINARY: usize = 32 * 1024 * 1024;
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshDaemon {
    Path,
    Managed,
}

impl SshAlias {
    pub fn with_daemon(mut self, daemon: SshDaemon) -> Self {
        self.desktop_daemon = Some(daemon);
        self
    }

    pub async fn probe_daemon(&self) -> Result<Option<SshDaemon>> {
        match self.bootstrap("probe", None).await?.as_slice() {
            b"path\n" => Ok(Some(SshDaemon::Path)),
            b"managed\n" => Ok(Some(SshDaemon::Managed)),
            b"missing\n" => Ok(None),
            _ => Err(ProtocolFault::new("BootstrapFailed", "unexpected SSH probe response").into()),
        }
    }

    pub async fn install_daemon(&self, binary: &[u8], sha256: &str) -> Result<()> {
        ensure!(
            !binary.is_empty() && binary.len() <= MAX_BINARY,
            "Linux daemon binary exceeds the transfer limit or is empty"
        );
        ensure!(
            sha256.len() == 64
                && sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid Linux daemon SHA-256"
        );
        let header = format!("{} {sha256}\n", binary.len());
        self.bootstrap("install", Some((header.as_bytes(), binary)))
            .await?;
        Ok(())
    }

    pub async fn ensure_daemon(&self, daemon: SshDaemon) -> Result<()> {
        let arguments = daemon_arguments(daemon, "ensure-daemon");
        self.bootstrap(&arguments, None).await?;
        Ok(())
    }

    async fn bootstrap(&self, arguments: &str, payload: Option<(&[u8], &[u8])>) -> Result<Vec<u8>> {
        let mut child = self
            .remote_command(&script_command(arguments))?
            .spawn()
            .context("starting Corvid SSH setup")?;
        let stdout = child.stdout.take().context("SSH stdout is unavailable")?;
        let stderr = child.stderr.take().context("SSH stderr is unavailable")?;
        let mut stdin = child.stdin.take().context("SSH stdin is unavailable")?;

        let send = async move {
            if let Some((header, binary)) = payload {
                stdin.write_all(header).await?;
                stdin.write_all(binary).await?;
            }
            Ok::<_, std::io::Error>(())
        };
        let mut output = Vec::new();
        let mut bounded = stdout.take((MAX_RESPONSE + 1) as u64);
        let (status, received, diagnostic, sent) = timeout(BOOTSTRAP_TIMEOUT, async {
            tokio::join!(
                child.wait(),
                bounded.read_to_end(&mut output),
                drain_diagnostic(stderr),
                send
            )
        })
        .await
        .context("SSH daemon setup timed out")?;
        let status = status?;
        if !status.success() {
            return Err(bootstrap_failure(status.code(), &diagnostic).into());
        }
        received?;
        sent?;
        ensure!(
            output.len() <= MAX_RESPONSE,
            "SSH daemon setup response is too large"
        );
        Ok(output)
    }
}

pub(super) fn daemon_command(daemon: SshDaemon, arguments: &str) -> String {
    script_command(&daemon_arguments(daemon, arguments))
}

fn daemon_arguments(daemon: SshDaemon, arguments: &str) -> String {
    let mode = match daemon {
        SshDaemon::Path => "path",
        SshDaemon::Managed => "managed",
    };
    format!("{mode} {arguments}")
}

fn script_command(arguments: &str) -> String {
    // The command and arguments are fixed. The artifact header and bytes travel over stdin.
    let script = SCRIPT.replace('\'', "'\\''");
    format!("sh -c '{script}' corvid-ssh {arguments}")
}

fn bootstrap_failure(code: Option<i32>, diagnostic: &[u8]) -> ProtocolFault {
    let (code, message) = match code {
        Some(255) => return ssh_failure(diagnostic),
        Some(70) => (
            "UnsupportedHost",
            "automatic installation requires a Linux x86_64 host",
        ),
        Some(71) => (
            "UnsafeInstallPath",
            "the remote Corvid home or runtime path is unsafe",
        ),
        Some(72) => (
            "InvalidDaemonBundle",
            "the remote daemon size or SHA-256 did not match",
        ),
        Some(74) => (
            "InstallationChanged",
            "a Corvid installation already exists. Connect again to use it",
        ),
        _ if String::from_utf8_lossy(diagnostic).contains("path must be shorter than SUN_LEN") => (
            "RuntimePathTooLong",
            "the remote runtime path is too long. Set a shorter private CORVID_RUNTIME_DIR",
        ),
        _ if String::from_utf8_lossy(diagnostic)
            .contains("unrecognized subcommand 'ensure-daemon'") =>
        {
            (
                "UpgradeRequired",
                "the installed Corvid cannot start over SSH. Update it or start it on the remote host",
            )
        }
        _ => (
            "BootstrapFailed",
            "unable to install or start the remote Corvid daemon",
        ),
    };
    ProtocolFault::new(code, message)
}
