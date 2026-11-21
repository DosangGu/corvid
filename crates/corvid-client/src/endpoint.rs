#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Result, bail, ensure};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::{COMMAND_TIMEOUT, Frame, PeerRole, ProtocolFault, Session};
use uuid::Uuid;

use crate::endpoint_time::timeout;

mod bootstrap;
pub(crate) mod native;
mod presentation;
pub use presentation::CompioPresentation;
mod process;
pub(crate) mod wire;
use wire::{Reader, Wire, Writer};
mod wsl;
pub use bootstrap::{DaemonInstallation, HostPlatform};
pub use wsl::WslDistribution;

const RELAY_SETUP_TIMEOUT: Duration = Duration::from_secs(10);
const DIAGNOSTIC_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_DIAGNOSTIC: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlEndpoint {
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    Local(PathBuf),
    Ssh(SshAlias),
    Wsl(WslDistribution),
}

#[derive(Clone, Debug)]
pub enum RemoteHost {
    Ssh(SshAlias),
    Wsl(WslDistribution),
}

impl RemoteHost {
    pub fn name(&self) -> &str {
        match self {
            Self::Ssh(alias) => alias.as_str(),
            Self::Wsl(distribution) => distribution.as_str(),
        }
    }

    pub fn endpoint(self) -> ControlEndpoint {
        match self {
            Self::Ssh(alias) => ControlEndpoint::Ssh(alias),
            Self::Wsl(distribution) => ControlEndpoint::Wsl(distribution),
        }
    }

    fn remote_command(&self, remote: &str) -> Result<Command> {
        match self {
            Self::Ssh(alias) => alias.remote_command(remote),
            Self::Wsl(distribution) => distribution.remote_command(remote),
        }
    }

    pub async fn interactive(&self, capabilities: Vec<String>) -> Result<InteractiveConnection> {
        self.clone().endpoint().interactive(capabilities).await
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SshAlias {
    name: String,
    desktop_daemon: Option<(HostPlatform, DaemonInstallation)>,
}

impl SshAlias {
    pub fn new(value: String) -> Result<Self> {
        ensure!(
            !value.is_empty()
                && value.len() <= 253
                && !value.starts_with('-')
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
            "invalid SSH alias"
        );
        Ok(Self {
            name: value,
            desktop_daemon: None,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.name
    }

    pub fn with_daemon(mut self, platform: HostPlatform, daemon: DaemonInstallation) -> Self {
        self.desktop_daemon = Some((platform, daemon));
        self
    }

    pub fn daemon(&self) -> Option<(HostPlatform, DaemonInstallation)> {
        self.desktop_daemon
    }

    fn command(&self, role: PeerRole) -> Result<Command> {
        let arguments = match role {
            PeerRole::Control => "control --stdio",
            PeerRole::Tui => "terminal-relay",
            _ => unreachable!("SSH only exposes the two daemon client roles"),
        };
        let remote = match self.desktop_daemon {
            Some((platform, daemon)) => bootstrap::daemon_command(platform, daemon, arguments),
            None => format!("corvid {arguments}"),
        };
        self.remote_command(&remote)
    }

    async fn connect_control(
        &self,
        capability: &str,
        controller: &str,
    ) -> Result<ControlConnection> {
        let command = self.command(PeerRole::Control)?;
        let error = match ControlConnection::relay(command, capability, controller).await {
            Ok(connection) => return Ok(connection),
            Err(error) => error,
        };
        if self.desktop_daemon.is_some() || !can_discover_daemon(&error) {
            return Err(error);
        }

        // Saved companion profiles have only the alias. The direct command also
        // supports SSH keys restricted to that exact command, so try it first.
        let host = RemoteHost::Ssh(self.clone());
        let Ok(Ok(platform)) = timeout(RELAY_SETUP_TIMEOUT, host.probe_platform()).await else {
            return Err(error);
        };
        let command = self.remote_command(&bootstrap::auto_control_command(platform))?;
        ControlConnection::relay(command, capability, controller).await
    }

    fn remote_command(&self, remote: &str) -> Result<Command> {
        let mut command = ssh_command()?;
        command.args([
            "-T",
            "-oBatchMode=yes",
            "-oStrictHostKeyChecking=yes",
            "-oConnectTimeout=10",
            "-oConnectionAttempts=1",
            "-oServerAliveInterval=5",
            "-oServerAliveCountMax=3",
            "-oForwardAgent=no",
            "-oForwardX11=no",
            "-oClearAllForwardings=yes",
            "-oControlMaster=no",
            "-oControlPath=none",
            "-oForkAfterAuthentication=no",
            "-oStdinNull=no",
            "-oPermitLocalCommand=no",
            "-oRemoteCommand=none",
            self.as_str(),
            remote,
        ]);
        command
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(command)
    }

    pub async fn interactive(&self, capabilities: Vec<String>) -> Result<InteractiveConnection> {
        InteractiveConnection::relay(self.command(PeerRole::Tui)?, capabilities).await
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn ssh_command() -> Result<Command> {
    Ok(Command::new("ssh"))
}

#[cfg(windows)]
fn ssh_command() -> Result<Command> {
    let mut command = Command::new(system_openssh()?);
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    Ok(command)
}

#[cfg(windows)]
pub fn check_system_openssh() -> Result<()> {
    ensure!(
        system_openssh()?.metadata()?.is_file(),
        "system OpenSSH is unavailable"
    );
    Ok(())
}

#[cfg(windows)]
fn system_openssh() -> Result<PathBuf> {
    Ok(system_directory()?.join(r"OpenSSH\ssh.exe"))
}

#[cfg(windows)]
fn system_directory() -> Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut directory = [0; 1024];
    // The buffer is writable for the supplied number of UTF-16 code units.
    let len =
        unsafe { GetSystemDirectoryW(directory.as_mut_ptr(), directory.len() as u32) } as usize;
    ensure!(
        (1..directory.len()).contains(&len),
        "Windows system directory is unavailable"
    );
    Ok(PathBuf::from(OsString::from_wide(&directory[..len])))
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub(crate) async fn local_wire(
    path: &Path,
    peer: PeerRole,
    capabilities: Vec<String>,
    controller: Option<String>,
) -> Result<Wire> {
    native::connect(path, peer, capabilities, controller)
        .await
        .map(Wire::from_native)
}

pub(crate) struct ControlConnection {
    pub(crate) wire: Wire,
}

impl ControlConnection {
    async fn relay(command: Command, capability: &str, controller: &str) -> Result<Self> {
        let capabilities = vec![capability.to_owned()];
        let controller = Some(controller.to_owned());
        let wire = native::relay(command, PeerRole::Control, capabilities, controller)
            .await
            .map(Wire::from_native)?;
        Ok(Self { wire })
    }
}

pub struct InteractiveConnection {
    session: Session,
    writer: Writer,
    incoming: Reader,
}

impl InteractiveConnection {
    fn new(wire: Wire) -> Self {
        Self {
            session: wire.session,
            writer: wire.writer,
            incoming: wire.reader,
        }
    }

    async fn relay(command: Command, capabilities: Vec<String>) -> Result<Self> {
        let wire = native::relay(command, PeerRole::Tui, capabilities, None)
            .await
            .map(Wire::from_native)?;
        Ok(Self::new(wire))
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub async fn send(&mut self, body: Body) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let message = self.session.envelope(Some(id.clone()), body);
        timeout(COMMAND_TIMEOUT, self.writer.envelope(&message)).await??;
        Ok(id)
    }

    pub async fn receive(&mut self) -> Result<Frame> {
        self.incoming.next_frame().await?.ok_or_else(|| {
            ProtocolFault::new("Offline", "daemon closed the interactive connection").into()
        })
    }

    pub async fn browse_directories(
        &mut self,
        query: corvid_protocol::wire::BrowseDirectories,
    ) -> Result<corvid_protocol::wire::DirectoryListing> {
        ensure!(
            self.session
                .capabilities
                .iter()
                .any(|cap| cap == corvid_protocol::WORKSPACE_DIRECTORY_CAPABILITY),
            ProtocolFault::new(
                "UnsupportedCapability",
                "Update Corvid on this host to choose a workspace folder."
            )
        );
        let id = self.send(Body::BrowseDirectories(query)).await?;
        let Frame::Structured(payload) = timeout(COMMAND_TIMEOUT, self.receive()).await?? else {
            bail!("unexpected terminal frame during folder lookup");
        };
        let reply = corvid_protocol::decode_envelope(&payload)?;
        self.session.validate_scope(&reply)?;
        ensure!(
            reply.request_id.as_deref() == Some(&id),
            "folder lookup response ID mismatch"
        );
        match reply.body {
            Some(Body::DirectoryListing(listing)) => Ok(listing),
            Some(Body::Error(error)) => Err(ProtocolFault::from_wire(&error).into()),
            _ => bail!("unexpected folder lookup response"),
        }
    }
}

impl ControlEndpoint {
    pub async fn interactive(&self, capabilities: Vec<String>) -> Result<InteractiveConnection> {
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            Self::Local(path) => {
                let wire = local_wire(path, PeerRole::Tui, capabilities, None).await?;
                Ok(InteractiveConnection::new(wire))
            }
            Self::Ssh(alias) => alias.interactive(capabilities).await,
            Self::Wsl(distribution) => {
                InteractiveConnection::relay(distribution.command(PeerRole::Tui)?, capabilities)
                    .await
            }
        }
    }

    pub(crate) async fn connect(
        &self,
        capability: &str,
        controller: Option<String>,
    ) -> Result<ControlConnection> {
        let controller = controller.unwrap_or_else(|| Uuid::new_v4().to_string());
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            Self::Local(path) => {
                let wire = local_wire(
                    path,
                    PeerRole::Control,
                    vec![capability.to_owned()],
                    Some(controller),
                )
                .await?;
                Ok(ControlConnection { wire })
            }
            Self::Ssh(alias) => alias.connect_control(capability, &controller).await,
            Self::Wsl(distribution) => {
                let command = distribution.command(PeerRole::Control)?;
                ControlConnection::relay(command, capability, &controller).await
            }
        }
    }
}

fn relay_failure(diagnostic: &[u8]) -> ProtocolFault {
    let message = String::from_utf8_lossy(diagnostic);
    if message.contains("Host key verification failed")
        || message.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
    {
        ProtocolFault::new("HostKeyRequired", "verify the host key using OpenSSH")
    } else if message.contains("Permission denied (") {
        ProtocolFault::new(
            "AuthenticationRequired",
            "SSH authentication needs attention",
        )
    } else if message.contains("corvid: not found") || message.contains("corvid: command not found")
    {
        ProtocolFault::new(
            "RelayUnavailable",
            "install corvid on the remote command PATH",
        )
    } else if message.contains("unrecognized subcommand 'terminal-relay'") {
        ProtocolFault::new("UpgradeRequired", "update Corvid on the execution host")
    } else {
        ProtocolFault::new("Offline", "Corvid relay connection failed")
    }
}

fn can_discover_daemon(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ProtocolFault>().is_some_and(|fault| {
        matches!(
            fault.code.as_str(),
            "RelayUnavailable" | "DaemonUnavailable" | "Offline"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_only_an_explicit_alias_and_fixed_remote_command() {
        for invalid in ["", "-F", "name\n", "name;ls", "user@host", "ssh://host"] {
            assert!(SshAlias::new(invalid.to_owned()).is_err());
        }
        let alias = SshAlias::new("work.remote-1".to_owned()).unwrap();
        let command = alias.command(PeerRole::Control).unwrap();
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args[0], "-T");
        assert_eq!(
            args[args.len() - 2..],
            ["work.remote-1", "corvid control --stdio"]
        );
        assert!(args.contains(&"-oBatchMode=yes".as_ref()));
        assert!(args.contains(&"-oStrictHostKeyChecking=yes".as_ref()));
    }

    #[test]
    fn classifies_without_leaking_relay_diagnostics() {
        let error = relay_failure(b"secret@host: Permission denied (publickey).");
        assert_eq!(error.code, "AuthenticationRequired");
        assert!(!error.message.contains("secret"));
        assert_eq!(
            relay_failure(b"Host key verification failed.").code,
            "HostKeyRequired"
        );
    }
}
