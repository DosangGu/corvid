use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_protocol::unix::SocketRole;
use corvid_protocol::{COMMAND_TIMEOUT, FrameError, PeerRole, ProtocolFault};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::process::{Child, ChildStderr, Command};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use uuid::Uuid;

use crate::connection::{Connection, open};

const SSH_SETUP_TIMEOUT: Duration = Duration::from_secs(10);
const DIAGNOSTIC_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_DIAGNOSTIC: usize = 4096;

#[derive(Clone, Debug)]
pub enum ControlEndpoint {
    Local(PathBuf),
    Ssh(SshAlias),
}

#[derive(Clone, Debug)]
pub struct SshAlias(String);

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
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn command(&self) -> Command {
        let mut command = Command::new("ssh");
        command.args([
            "-T",
            "-oBatchMode=yes",
            "-oStrictHostKeyChecking=yes",
            "-oConnectTimeout=10",
            "-oConnectionAttempts=1",
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
            "corvid control --stdio",
        ]);
        command
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }
}

type Input = Box<dyn AsyncRead + Send + Unpin>;
type Output = Box<dyn AsyncWrite + Send + Unpin>;

pub(crate) struct ControlConnection {
    pub(crate) wire: Connection<Input, Output>,
    _ssh: Option<SshProcess>,
}

impl ControlEndpoint {
    pub(crate) async fn connect(
        &self,
        capability: &str,
        controller: Option<String>,
    ) -> Result<ControlConnection> {
        let (read, write, mut ssh): (Input, Output, _) = match self {
            Self::Local(path) => {
                let (read, write) = open(path, SocketRole::Control).await?.into_split();
                (Box::new(read), Box::new(write), None)
            }
            Self::Ssh(alias) => {
                let mut child = alias
                    .command()
                    .spawn()
                    .context("starting SSH control link")?;
                let read = child.stdout.take().context("SSH stdout is unavailable")?;
                let write = child.stdin.take().context("SSH stdin is unavailable")?;
                let stderr = child.stderr.take().context("SSH stderr is unavailable")?;
                let diagnostic = tokio::spawn(drain_diagnostic(stderr));
                (
                    Box::new(read),
                    Box::new(write),
                    Some(SshProcess { child, diagnostic }),
                )
            }
        };

        let controller = controller.unwrap_or_else(|| Uuid::new_v4().to_string());
        let negotiate = Connection::negotiate(
            read,
            write,
            PeerRole::Control,
            vec![capability.to_owned()],
            Some(controller),
        );
        let wire = match &mut ssh {
            Some(process) => match timeout(SSH_SETUP_TIMEOUT, negotiate).await {
                Ok(Ok(wire)) => wire,
                Ok(Err(error)) => return Err(process.classify(error).await),
                Err(_) => return Err(ProtocolFault::new("Offline", "SSH setup timed out").into()),
            },
            None => timeout(COMMAND_TIMEOUT, negotiate)
                .await
                .context("daemon handshake timed out")??,
        };
        Ok(ControlConnection { wire, _ssh: ssh })
    }
}

struct SshProcess {
    child: Child,
    diagnostic: JoinHandle<Vec<u8>>,
}

impl SshProcess {
    async fn classify(&mut self, error: anyhow::Error) -> anyhow::Error {
        if error.is::<ProtocolFault>() {
            return error;
        }
        if matches!(error.downcast_ref(), Some(FrameError::ProtocolMismatch)) {
            return ProtocolFault::new("ProtocolMismatch", "SSH stdout has an invalid preamble")
                .into();
        }
        let diagnostic = timeout(DIAGNOSTIC_TIMEOUT, async {
            if self.child.wait().await.ok()?.success() {
                return None;
            }
            (&mut self.diagnostic).await.ok()
        })
        .await;
        match diagnostic {
            Ok(Some(bytes)) => ssh_failure(&bytes).into(),
            _ => error,
        }
    }
}

impl Drop for SshProcess {
    fn drop(&mut self) {
        self.diagnostic.abort();
    }
}

async fn drain_diagnostic(mut stderr: ChildStderr) -> Vec<u8> {
    let mut tail = Vec::with_capacity(MAX_DIAGNOSTIC);
    let mut buffer = [0; 1024];
    while let Ok(count @ 1..) = stderr.read(&mut buffer).await {
        let discard = (tail.len() + count).saturating_sub(MAX_DIAGNOSTIC);
        tail.drain(..discard);
        tail.extend_from_slice(&buffer[..count]);
    }
    tail
}

fn ssh_failure(diagnostic: &[u8]) -> ProtocolFault {
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
    } else {
        ProtocolFault::new("Offline", "SSH control connection failed")
    }
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
        let command = alias.command();
        let args: Vec<_> = command.as_std().get_args().collect();
        assert_eq!(args[0], "-T");
        assert_eq!(
            args[args.len() - 2..],
            ["work.remote-1", "corvid control --stdio"]
        );
        assert!(args.contains(&"-oBatchMode=yes".as_ref()));
        assert!(args.contains(&"-oStrictHostKeyChecking=yes".as_ref()));
    }

    #[test]
    fn classifies_without_leaking_ssh_diagnostics() {
        let error = ssh_failure(b"secret@host: Permission denied (publickey).");
        assert_eq!(error.code, "AuthenticationRequired");
        assert!(!error.message.contains("secret"));
        assert_eq!(
            ssh_failure(b"Host key verification failed.").code,
            "HostKeyRequired"
        );
    }
}
