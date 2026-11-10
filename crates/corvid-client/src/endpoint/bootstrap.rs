use std::time::Duration;

use anyhow::{Context, Result, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use corvid_protocol::ProtocolFault;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use super::{SshAlias, drain_diagnostic, ssh_failure};

const LINUX_SCRIPT: &str = include_str!("bootstrap.sh");
const WINDOWS_SCRIPT: &str = include_str!("bootstrap_windows.ps1");
const WINDOWS_INSTALL: &str = include_str!("install_windows.ps1");
const WINDOWS_PROBE: &str = r#"
if (PathDaemon) { [Console]::WriteLine('path') }
elseif (ManagedDaemon) { [Console]::WriteLine('managed') }
else { RequireX64; [Console]::WriteLine('missing') }
"#;
const WINDOWS_PLATFORM: &str = r#"
if ($env:OS -ne 'Windows_NT') { exit 70 }
[Console]::WriteLine('Windows')
"#;
const MAX_RESPONSE: usize = 64;
const MAX_BINARY: usize = 32 * 1024 * 1024;
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshDaemon {
    Path,
    Managed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshPlatform {
    Linux,
    Windows,
}

impl SshAlias {
    pub fn with_daemon(mut self, platform: SshPlatform, daemon: SshDaemon) -> Self {
        self.desktop_daemon = Some((platform, daemon));
        self
    }

    pub async fn probe_platform(&self) -> Result<SshPlatform> {
        // Start with a single shell-neutral read-only command. Never send the
        // POSIX bootstrap to a host that has not identified itself as Linux.
        match self.bootstrap("uname -s", None).await {
            Ok(output) if response(&output) == b"Linux" => return Ok(SshPlatform::Linux),
            Err(error) if !setup_unavailable(&error) => return Err(error),
            _ => {}
        }
        let command = windows_command(WINDOWS_PLATFORM);
        match self.bootstrap(&command, None).await {
            Ok(output) if response(&output) == b"Windows" => Ok(SshPlatform::Windows),
            Err(error) if !setup_unavailable(&error) => Err(error),
            _ => Err(unsupported_host().into()),
        }
    }

    pub async fn probe_daemon(&self, platform: SshPlatform) -> Result<Option<SshDaemon>> {
        let command = match platform {
            SshPlatform::Linux => linux_command("probe"),
            SshPlatform::Windows => windows_setup(WINDOWS_PROBE),
        };
        match response(&self.bootstrap(&command, None).await?) {
            b"path" => Ok(Some(SshDaemon::Path)),
            b"managed" => Ok(Some(SshDaemon::Managed)),
            b"missing" => Ok(None),
            _ => Err(ProtocolFault::new("BootstrapFailed", "unexpected SSH probe response").into()),
        }
    }

    pub async fn install_daemon(
        &self,
        platform: SshPlatform,
        binary: &[u8],
        sha256: &str,
    ) -> Result<()> {
        ensure!(
            !binary.is_empty() && binary.len() <= MAX_BINARY,
            "daemon binary exceeds the transfer limit or is empty"
        );
        ensure!(
            sha256.len() == 64
                && sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid daemon SHA-256"
        );
        match platform {
            SshPlatform::Linux => {
                let header = format!("{} {sha256}\n", binary.len());
                self.bootstrap(&linux_command("install"), Some((header.as_bytes(), binary)))
                    .await?;
            }
            SshPlatform::Windows => {
                let archive = windows_archive(binary)?;
                let script = windows_install(binary.len(), sha256);
                self.bootstrap(&windows_setup(&script), Some((&[], &archive)))
                    .await?;
            }
        }
        Ok(())
    }

    pub async fn ensure_daemon(&self, platform: SshPlatform, daemon: SshDaemon) -> Result<()> {
        let arguments = match platform {
            SshPlatform::Linux => "ensure-daemon",
            SshPlatform::Windows => "ensure-daemon --ssh",
        };
        self.bootstrap(&daemon_command(platform, daemon, arguments), None)
            .await?;
        Ok(())
    }

    async fn bootstrap(&self, command: &str, payload: Option<(&[u8], &[u8])>) -> Result<Vec<u8>> {
        let mut child = self
            .remote_command(command)?
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

pub(super) fn daemon_command(platform: SshPlatform, daemon: SshDaemon, arguments: &str) -> String {
    match platform {
        SshPlatform::Linux => {
            let mode = match daemon {
                SshDaemon::Path => "path",
                SshDaemon::Managed => "managed",
            };
            linux_command(&format!("{mode} {arguments}"))
        }
        SshPlatform::Windows => {
            let resolve = match daemon {
                SshDaemon::Path => "PathDaemon",
                SshDaemon::Managed => "ManagedDaemon",
            };
            windows_setup(&format!("RunDaemon ({resolve}) '{arguments}'"))
        }
    }
}

fn linux_command(arguments: &str) -> String {
    // The command and arguments are fixed. The artifact header and bytes travel over stdin.
    let script = LINUX_SCRIPT.replace('\'', "'\\''");
    format!("sh -c '{script}' corvid-ssh {arguments}")
}

fn windows_setup(action: &str) -> String {
    windows_command(&format!("{WINDOWS_SCRIPT}\n{action}"))
}

fn windows_install(size: usize, sha256: &str) -> String {
    format!("$size = {size}\n$sha = '{sha256}'\n{WINDOWS_INSTALL}")
}

fn windows_archive(binary: &[u8]) -> Result<Vec<u8>> {
    let mut archive = tar::Builder::new(Vec::with_capacity(binary.len() + 2048));
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(binary.len() as u64);
    header.set_mode(0o600);
    archive.append_data(&mut header, "corvid.exe", binary)?;
    Ok(archive.into_inner()?)
}

fn windows_command(script: &str) -> String {
    // A fixed expression with an ASCII payload works through cmd.exe and
    // PowerShell. UTF-16LE -EncodedCommand would exceed cmd.exe's length limit.
    let encoded = STANDARD.encode(script);
    format!(
        "powershell.exe -NoLogo -NoProfile -NonInteractive -Command \"iex([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded}')))\""
    )
}

fn response(output: &[u8]) -> &[u8] {
    output
        .strip_suffix(b"\r\n")
        .or_else(|| output.strip_suffix(b"\n"))
        .unwrap_or(output)
}

fn setup_unavailable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ProtocolFault>()
        .is_some_and(|fault| matches!(fault.code.as_str(), "BootstrapFailed" | "UnsupportedHost"))
}

fn unsupported_host() -> ProtocolFault {
    ProtocolFault::new(
        "UnsupportedHost",
        "automatic installation requires a Linux x86_64 or Windows x64 SSH host",
    )
}

fn bootstrap_failure(code: Option<i32>, diagnostic: &[u8]) -> ProtocolFault {
    let (code, message) = match code {
        Some(255) => return ssh_failure(diagnostic),
        Some(70) => return unsupported_host(),
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
        Some(75) => (
            "WindowsTarUnavailable",
            "Windows SSH installation requires the system tar.exe",
        ),
        _ if String::from_utf8_lossy(diagnostic).contains("path must be shorter than SUN_LEN") => (
            "RuntimePathTooLong",
            "the remote runtime path is too long. Set a shorter private CORVID_RUNTIME_DIR",
        ),
        _ if String::from_utf8_lossy(diagnostic)
            .contains("unrecognized subcommand 'ensure-daemon'")
            || String::from_utf8_lossy(diagnostic).contains("unexpected argument '--ssh'") =>
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_commands_fit_the_default_ssh_shell_limit() {
        assert!(windows_setup(&windows_install(MAX_BINARY, &"0".repeat(64))).len() < 8000);
        assert!(
            daemon_command(SshPlatform::Windows, SshDaemon::Managed, "control --stdio").len()
                < 8000
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_install_verifies_before_publishing_and_preserves_existing_files() {
        use std::io::Write;
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};

        let directory = tempfile::tempdir().unwrap();
        let managed = directory.path().join("Corvid-SSH/corvid.exe");
        let invoke = |action: &str, input: &[u8]| {
            // Exercise cmd.exe quoting without using the user's install directory.
            let action = format!(
                "$base = $env:CORVID_TEST_INSTALL_ROOT\nfunction PathDaemon {{}}\n{action}"
            );
            let command = windows_setup(&action);
            let mut child = Command::new("cmd.exe")
                .args(["/D", "/S", "/C"])
                .raw_arg(format!("\"{command}\""))
                .env("CORVID_TEST_INSTALL_ROOT", directory.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(input).unwrap();
            child.wait_with_output().unwrap()
        };
        let assert_status = |output: &std::process::Output, code| {
            assert_eq!(
                output.status.code(),
                Some(code),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        let digest = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let probe = invoke(WINDOWS_PROBE, b"");
        assert_status(&probe, 0);
        assert_eq!(response(&probe.stdout), b"missing");
        for (input, sha) in [(&b"ab"[..], digest), (&b"abc"[..], &"0".repeat(64))] {
            let script = windows_install(3, sha);
            assert_status(&invoke(&script, &windows_archive(input).unwrap()), 72);
            assert!(!managed.exists());
        }
        let script = windows_install(3, digest);
        let archive = windows_archive(b"abc").unwrap();
        assert_status(&invoke(&script, &archive), 0);
        assert_status(&invoke(&script, &archive), 0);
        assert_eq!(std::fs::read(&managed).unwrap(), b"abc");
        let probe = invoke(WINDOWS_PROBE, b"");
        assert_status(&probe, 0);
        assert_eq!(response(&probe.stdout), b"managed");

        std::fs::write(&managed, b"existing").unwrap();
        assert_status(&invoke(&script, &archive), 74);
        assert_eq!(std::fs::read(&managed).unwrap(), b"existing");
    }
}
