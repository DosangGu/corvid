//! UI-independent monitoring, started by a verified Desktop or TUI connection.
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_client::{ControlEndpoint, DaemonInstallation, HostPlatform, RemoteHost};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use crate::platform::StateDirectory;
use crate::{Config, HostEndpoint, PresentationAddress, PresentationClient};

pub const CAPABILITY: &str = "automatic_monitor.v1";
const LIMIT: u64 = 8192;

pub struct Scope {
    pub state: PathBuf,
    pub address: <PresentationAddress as ToOwned>::Owned,
    pub config: Config,
}

/// Discover existing monitor scopes without starting services or remote connections.
pub fn discover(root: &Path) -> Result<Vec<Scope>> {
    let mut paths = vec![root.to_owned()];
    match std::fs::read_dir(root.join("slot-board")) {
        Ok(entries) => {
            let mut scoped: Vec<_> = entries
                .take(256)
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| Uuid::parse_str(name).is_ok())
                })
                .map(|entry| entry.path())
                .collect();
            scoped.sort();
            paths.extend(scoped);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut scopes = Vec::new();
    for state in paths {
        let Some(config) = Config::read_if_present(&state)? else {
            continue;
        };
        let shared = state == root;
        if !shared
            && state.file_name().and_then(|name| name.to_str())
                != Some(config.host_id.simple().to_string().as_str())
        {
            continue;
        }
        let address = saved_address(&state, &config, shared)?;
        scopes.push(Scope {
            state,
            address,
            config,
        });
    }
    Ok(scopes)
}

#[derive(Deserialize, Serialize)]
struct Connection {
    schema_version: u32,
    host: Uuid,
    controller: Uuid,
    endpoint: HostEndpoint,
    daemon: Option<(HostPlatform, DaemonInstallation)>,
    address: <PresentationAddress as ToOwned>::Owned,
}

impl Scope {
    pub fn prepare(
        root: &Path,
        endpoint: &ControlEndpoint,
        host: Uuid,
        instance: &str,
    ) -> Result<Self> {
        let saved = HostEndpoint::from(endpoint);
        let shared = Config::read_if_present(root)?
            .is_some_and(|config| config.host_id == host && config.endpoint == saved);
        let state = if shared {
            root.to_owned()
        } else {
            root.join("slot-board").join(host.simple().to_string())
        };
        let config = match Config::read_if_present(&state)? {
            Some(config) => config,
            None => {
                let initialized = crate::initialize(&state, host, saved, 8);
                match Config::read_if_present(&state)? {
                    Some(config) => config,
                    None => {
                        initialized?;
                        anyhow::bail!("agent monitor configuration is missing");
                    }
                }
            }
        };
        ensure!(
            config.host_id == host,
            "agent monitor belongs to another host"
        );
        let address = address(&config, shared, instance)?;
        Ok(Self {
            state,
            address,
            config,
        })
    }

    /// Publish only a connection whose host identity the caller has verified.
    pub fn configure(&self, endpoint: &ControlEndpoint) -> Result<()> {
        let daemon = match endpoint {
            ControlEndpoint::Ssh(alias) => alias.daemon(),
            ControlEndpoint::Wsl(distribution) => distribution
                .daemon()
                .map(|daemon| (HostPlatform::Linux, daemon)),
            ControlEndpoint::Local(_) => None,
        };
        let record = Connection {
            schema_version: 1,
            host: self.config.host_id,
            controller: self.config.controller_id,
            endpoint: HostEndpoint::from(endpoint),
            daemon,
            address: self.address.clone(),
        };
        let directory = StateDirectory::create_all(&self.state.join("monitor"))?;
        let _lock = directory.lock_storage()?;
        let bytes = serde_json::to_vec(&record)?;
        ensure!(
            bytes.len() as u64 <= LIMIT,
            "agent monitor configuration exceeds its limit"
        );
        if directory
            .read_private_file("connection.json", LIMIT)?
            .as_deref()
            != Some(&bytes)
        {
            directory.replace_private_file("connection.json", &bytes)?;
        }
        Ok(())
    }

    pub async fn ensure_running(&self, executable: &Path) -> Result<()> {
        if self.connected().await? {
            return Ok(());
        }
        let mut command = Command::new(executable);
        command.arg("companion").arg("--state-dir").arg(&self.state);
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Threading::{
                CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
            };
            let mut flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
            if std::env::var_os("SSH_CONNECTION").is_some() {
                flags |= CREATE_BREAKAWAY_FROM_JOB;
            }
            command
                .arg("--pipe-instance")
                .arg(&self.address)
                .creation_flags(flags);
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            command.arg("--socket-dir").arg(&self.address);
            // Detach from the calling TTY before exec, using only signal-safe operations.
            unsafe {
                command.pre_exec(|| {
                    if nix::libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        command
            .arg("run")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().context("starting the agent monitor")?;
        timeout(Duration::from_secs(5), async {
            loop {
                if self.connected().await? {
                    return Ok::<_, anyhow::Error>(());
                }
                // Concurrent starters can lose the store lock before the winner publishes IPC.
                let _ = child.try_wait()?;
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("agent monitor did not start")?
    }

    async fn connected(&self) -> Result<bool> {
        let Some(mut client) = PresentationClient::connect_if_present(&self.address).await? else {
            return Ok(false);
        };
        ensure!(
            client
                .session()
                .capabilities
                .iter()
                .any(|cap| cap == CAPABILITY),
            "An older companion is running. Quit its companion service once, then reconnect Desktop or TUI."
        );
        timeout(Duration::from_secs(5), client.slots(Some(&self.config))).await??;
        Ok(true)
    }
}

pub fn address(
    config: &Config,
    shared: bool,
    _instance: &str,
) -> Result<<PresentationAddress as ToOwned>::Owned> {
    #[cfg(windows)]
    return Ok(if shared {
        _instance.to_owned()
    } else {
        config.controller_id.simple().to_string()
    });
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        #[cfg(target_os = "macos")]
        let runtime = corvid_client::runtime_path(None)?;
        #[cfg(target_os = "linux")]
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                // SSH sessions may omit XDG_RUNTIME_DIR even with a running user manager.
                let uid = unsafe { nix::libc::geteuid() };
                let runtime = PathBuf::from(format!("/run/user/{uid}"));
                runtime.is_dir().then_some(runtime)
            })
            .or_else(|| match &config.endpoint {
                HostEndpoint::Local { runtime_dir } => Some(runtime_dir.clone()),
                _ => corvid_client::runtime_path(None).ok(),
            })
            .context("set XDG_RUNTIME_DIR for the agent monitor")?;
        let name = if shared {
            "corvid-companion".to_owned()
        } else if cfg!(target_os = "macos") {
            format!("s-{}", config.controller_id.simple())
        } else {
            format!("cv-slot-{}", config.controller_id.simple())
        };
        Ok(runtime.join(name))
    }
}

fn read(state: &Path, config: &Config) -> Result<Option<Connection>> {
    let path = state.join("monitor");
    if !path.try_exists()? {
        return Ok(None);
    }
    let directory = StateDirectory::open(&path)?;
    let Some(bytes) = directory.read_private_file("connection.json", LIMIT)? else {
        return Ok(None);
    };
    let saved: Connection = serde_json::from_slice(&bytes)?;
    ensure!(
        saved.schema_version == 1
            && saved.host == config.host_id
            && saved.controller == config.controller_id,
        "agent monitor connection identity changed"
    );
    Ok(Some(saved))
}

pub fn saved_address(
    state: &Path,
    config: &Config,
    shared: bool,
) -> Result<<PresentationAddress as ToOwned>::Owned> {
    match read(state, config)? {
        Some(saved) => Ok(saved.address),
        None => address(config, shared, "main"),
    }
}

pub(crate) fn endpoint(state: &Path, config: &Config) -> Result<Option<ControlEndpoint>> {
    let Some(saved) = read(state, config)? else {
        return Ok(None);
    };
    let endpoint = saved.endpoint.control_endpoint()?;
    Ok(Some(match (endpoint, saved.daemon) {
        (ControlEndpoint::Ssh(alias), Some((platform, daemon))) => {
            ControlEndpoint::Ssh(alias.with_daemon(platform, daemon))
        }
        (ControlEndpoint::Wsl(distribution), Some((HostPlatform::Linux, daemon))) => {
            RemoteHost::Wsl(distribution)
                .with_daemon(HostPlatform::Linux, daemon)
                .endpoint()
        }
        (endpoint, None) => endpoint,
        _ => anyhow::bail!("invalid agent monitor daemon platform"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use corvid_client::SshAlias;

    #[test]
    fn discovers_and_reuses_host_scopes_without_starting_services() -> Result<()> {
        #[cfg(windows)]
        let directory = tempfile::tempdir()?;
        #[cfg(not(windows))]
        let directory = tempfile::tempdir_in(std::env::home_dir().context("home")?)?;
        let root = directory.path().join("companion");
        let endpoint = ControlEndpoint::Local(directory.path().join("runtime"));
        let host = Uuid::new_v4();
        let first = Scope::prepare(&root, &endpoint, host, "main")?;
        first.configure(&endpoint)?;
        let again = Scope::prepare(&root, &endpoint, host, "main")?;
        assert_eq!(first.config.controller_id, again.config.controller_id);
        let other = Uuid::new_v4();
        let second = Scope::prepare(&root, &endpoint, other, "main")?;
        assert_ne!(first.config.controller_id, second.config.controller_id);
        let wrong = root
            .join("slot-board")
            .join(Uuid::new_v4().simple().to_string());
        crate::initialize(&wrong, host, HostEndpoint::from(&endpoint), 8)?;
        let scopes = discover(&root)?;
        assert_eq!(scopes.len(), 2);
        assert!(
            scopes
                .iter()
                .any(|scope| scope.config.controller_id == first.config.controller_id)
        );
        assert!(
            scopes
                .iter()
                .any(|scope| scope.config.controller_id == second.config.controller_id)
        );
        let direct = discover(&first.state)?;
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].address, first.address);
        Ok(())
    }

    #[test]
    fn retains_verified_remote_installation_and_rejects_another_controller() -> Result<()> {
        #[cfg(windows)]
        let directory = tempfile::tempdir()?;
        #[cfg(not(windows))]
        let directory = tempfile::tempdir_in(std::env::home_dir().context("home")?)?;
        let state = directory.path().join("state");
        let host = Uuid::new_v4();
        let remote = ControlEndpoint::Ssh(
            SshAlias::new("work-host".into())?
                .with_daemon(HostPlatform::Linux, DaemonInstallation::Managed),
        );
        crate::initialize(&state, host, HostEndpoint::from(&remote), 8)?;
        let config = Config::read_if_present(&state)?.context("config")?;
        #[cfg(windows)]
        let address = config.controller_id.simple().to_string();
        #[cfg(not(windows))]
        let address = directory.path().join("presentation");
        let scope = Scope {
            state,
            address,
            config,
        };
        scope.configure(&remote)?;
        assert_eq!(endpoint(&scope.state, &scope.config)?, Some(remote));
        assert_eq!(
            saved_address(&scope.state, &scope.config, false)?,
            scope.address
        );
        let mut other = Config::read_if_present(&scope.state)?.context("config")?;
        other.controller_id = Uuid::new_v4();
        assert!(endpoint(&scope.state, &other).is_err());
        Ok(())
    }
}
