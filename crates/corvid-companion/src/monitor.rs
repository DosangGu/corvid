//! UI-independent monitoring, started by a verified Desktop or TUI connection.
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_async::time::{sleep, timeout};
use corvid_client::{ControlEndpoint, DaemonInstallation, HostPlatform, RemoteHost};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{CompanionConnection, PresentationSlots, StopAgentMonitor};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::Command;
use uuid::Uuid;

use crate::platform::{StateDirectory, StateLock};
use crate::{Config, HostEndpoint, PresentationAddress, PresentationClient};

pub const CAPABILITY: &str = "automatic_monitor.v1";
pub(crate) const EPOCH_RETIREMENT_CAPABILITY: &str = "retire_previous_epoch_slots.v1";
pub(crate) const DECK_PRIORITY_CAPABILITY: &str = "deck_priority.v1";
pub(crate) const PROCESS_EXIT_RETIREMENT_CAPABILITY: &str = "retire_exited_process_slots.v1";
const LIMIT: u64 = 8192;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_POLL: Duration = Duration::from_millis(50);

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
    pub fn control_endpoint(&self) -> Result<ControlEndpoint> {
        match endpoint(&self.state, &self.config)? {
            Some(endpoint) => Ok(endpoint),
            None => self.config.control_endpoint(),
        }
    }

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
        self.start_monitor(executable, false).await
    }

    pub async fn reconnect(&self) -> Result<()> {
        if let Some((mut client, _)) = self.connected().await? {
            timeout(STARTUP_TIMEOUT, client.reconnect_monitor()).await??;
        }
        Ok(())
    }

    pub async fn refresh_running(&self, executable: &Path) -> Result<()> {
        self.start_monitor(executable, true).await
    }

    async fn start_monitor(&self, executable: &Path, running_only: bool) -> Result<()> {
        let directory = StateDirectory::create_all(&self.state.join("monitor"))?;
        let _startup = wait_for_lock(|| directory.lock_startup())
            .await
            .context("waiting for agent monitor startup")?;
        if let Some((mut client, slots)) = self.connected().await? {
            if self.supports_monitor(&client)
                && client
                    .session()
                    .daemon_version
                    .as_deref()
                    .and_then(|v| semver::Version::parse(v).ok())
                    .is_some_and(|v| {
                        v >= semver::Version::parse(env!("CARGO_PKG_VERSION"))
                            .expect("valid workspace version")
                    })
            {
                if !running_only
                    && needs_wake(&self.config.endpoint, slots.connection)
                    && client
                        .session()
                        .capabilities
                        .iter()
                        .any(|capability| capability == crate::ipc::MONITOR_RECONNECT_CAPABILITY)
                {
                    timeout(STARTUP_TIMEOUT, client.reconnect_monitor()).await??;
                }
                return Ok(());
            }
            let reply = timeout(
                STARTUP_TIMEOUT,
                client.request(Body::StopAgentMonitor(StopAgentMonitor {})),
            )
            .await
            .context("waiting for the previous agent monitor to accept shutdown")??;
            ensure!(
                matches!(reply, Body::Pong(_)),
                "unexpected agent monitor shutdown response"
            );
            drop(client);

            let state = StateDirectory::open(&self.state)?;
            // The service holds this lock until it flushes observations and closes IPC.
            let _retired = wait_for_lock(|| state.lock_storage())
                .await
                .context("waiting for the previous agent monitor to exit")?;
        } else if running_only {
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
        let mut child = async_process::Command::from(command)
            .arg("run")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("starting the agent monitor")?;
        timeout(STARTUP_TIMEOUT, async {
            loop {
                if let Some((client, _)) = self.connected().await? {
                    ensure!(
                        self.supports_monitor(&client),
                        "the started agent monitor does not support the current slot layout"
                    );
                    return Ok::<_, anyhow::Error>(());
                }
                // Concurrent starters can lose the store lock before the winner publishes IPC.
                let _ = child.try_status()?;
                sleep(STARTUP_POLL).await;
            }
        })
        .await
        .context("agent monitor did not start")?
    }

    async fn connected(&self) -> Result<Option<(PresentationClient, PresentationSlots)>> {
        let Some(mut client) = PresentationClient::connect_if_present(&self.address).await? else {
            return Ok(None);
        };
        ensure!(
            client
                .session()
                .capabilities
                .iter()
                .any(|cap| cap == CAPABILITY),
            "An older companion is running. Quit its companion service once, then reconnect Desktop or TUI."
        );
        let slots = timeout(STARTUP_TIMEOUT, client.slots(Some(&self.config))).await??;
        Ok(Some((client, slots)))
    }

    fn supports_monitor(&self, client: &PresentationClient) -> bool {
        supports_current_layout(client)
            && (!matches!(self.config.endpoint, HostEndpoint::Ssh { .. })
                || client
                    .session()
                    .capabilities
                    .iter()
                    .any(|capability| capability == crate::ipc::MONITOR_RECONNECT_CAPABILITY))
    }
}

fn needs_wake(endpoint: &HostEndpoint, connection: i32) -> bool {
    match CompanionConnection::try_from(connection) {
        Ok(CompanionConnection::Offline | CompanionConnection::Error) => true,
        Ok(CompanionConnection::Connecting) => matches!(endpoint, HostEndpoint::Ssh { .. }),
        Ok(
            CompanionConnection::Unspecified
            | CompanionConnection::Ready
            | CompanionConnection::Syncing
            | CompanionConnection::Incompatible,
        )
        | Err(_) => false,
    }
}

fn supports_current_layout(client: &PresentationClient) -> bool {
    [
        EPOCH_RETIREMENT_CAPABILITY,
        DECK_PRIORITY_CAPABILITY,
        PROCESS_EXIT_RETIREMENT_CAPABILITY,
        crate::PRESENTATION_RUNS_CAPABILITY,
    ]
    .into_iter()
    .all(|required| {
        client
            .session()
            .capabilities
            .iter()
            .any(|cap| cap == required)
    })
}

async fn wait_for_lock(mut try_lock: impl FnMut() -> Result<StateLock>) -> Result<StateLock> {
    timeout(STARTUP_TIMEOUT, async {
        loop {
            match try_lock() {
                Ok(lock) => return Ok(lock),
                Err(error) if lock_busy(&error) => sleep(STARTUP_POLL).await,
                Err(error) => return Err(error),
            }
        }
    })
    .await?
}

fn lock_busy(error: &anyhow::Error) -> bool {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    return error.downcast_ref::<nix::errno::Errno>() == Some(&nix::errno::Errno::EWOULDBLOCK);

    #[cfg(windows)]
    return error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        error.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32)
    });
}

pub fn address(
    config: &Config,
    shared: bool,
    _instance: &str,
) -> Result<<PresentationAddress as ToOwned>::Owned> {
    use corvid_protocol::paths;

    let controller = (!shared).then_some(config.controller_id);
    #[cfg(windows)]
    return Ok(paths::monitor_pipe_instance(_instance, controller));
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let local_runtime = match &config.endpoint {
            HostEndpoint::Local { runtime_dir } => Some(runtime_dir.as_path()),
            HostEndpoint::Ssh { .. } | HostEndpoint::Wsl { .. } => None,
        };
        paths::monitor_socket(controller, local_runtime)?.prepare()
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

    #[test]
    fn wakes_only_offline_error_or_ssh_connecting_monitors() {
        let local = HostEndpoint::Local {
            runtime_dir: PathBuf::from("/tmp/corvid-test"),
        };
        let ssh = HostEndpoint::Ssh {
            alias: "work-host".into(),
        };
        assert!(needs_wake(&local, CompanionConnection::Offline as i32));
        assert!(needs_wake(&local, CompanionConnection::Error as i32));
        assert!(!needs_wake(&local, CompanionConnection::Connecting as i32));
        assert!(needs_wake(&ssh, CompanionConnection::Connecting as i32));
        for state in [
            CompanionConnection::Ready,
            CompanionConnection::Syncing,
            CompanionConnection::Incompatible,
        ] {
            assert!(!needs_wake(&local, state as i32));
            assert!(!needs_wake(&ssh, state as i32));
        }
    }

    #[cfg(unix)]
    #[compio::test]
    async fn ensure_running_wakes_offline_monitor_but_refresh_running_does_not() -> Result<()> {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use compio::io::util::Splittable;
        use corvid_protocol::unix::{RuntimeDir, SocketRole};
        use corvid_protocol::wire::{Envelope, Pong, PresentationSlots};
        use corvid_protocol::{
            CompioFrameReader, CompioFrameWriter, FrameRole, PeerRole, accept_hello_with_profiles,
        };

        let directory = tempfile::tempdir_in(std::env::home_dir().context("home")?)?;
        let state = directory.path().join("state");
        let address = directory.path().join("ipc");
        let host = Uuid::new_v4();
        let remote = ControlEndpoint::Ssh(SshAlias::new("work-host".into())?);
        crate::initialize(&state, host, HostEndpoint::from(&remote), 8)?;
        let config = Config::read_if_present(&state)?.context("config")?;
        let controller = config.controller_id.to_string();
        let scope = Scope {
            state,
            address: address.clone(),
            config,
        };

        let runtime = RuntimeDir::create_all(&address)?;
        let instance = runtime.lock_instance()?;
        let listener = instance.bind_compio(SocketRole::Presentation).await?;

        let connection_state = Arc::new(AtomicUsize::new(CompanionConnection::Offline as usize));
        let reconnect_count = Arc::new(AtomicUsize::new(0));
        let server_state = Arc::clone(&connection_state);
        let server_reconnects = Arc::clone(&reconnect_count);
        let host_str = host.to_string();
        let epoch = Uuid::new_v4().to_string();

        let server = async {
            let caps = [
                CAPABILITY,
                EPOCH_RETIREMENT_CAPABILITY,
                DECK_PRIORITY_CAPABILITY,
                PROCESS_EXIT_RETIREMENT_CAPABILITY,
                crate::PRESENTATION_CAPABILITY,
                crate::PRESENTATION_RUNS_CAPABILITY,
                crate::ipc::MONITOR_RECONNECT_CAPABILITY,
            ];
            while let Ok(Some(stream)) = listener.accept().await {
                let state = Arc::clone(&server_state);
                let reconnects = Arc::clone(&server_reconnects);
                let controller = controller.clone();
                let host_str = host_str.clone();
                let epoch = epoch.clone();
                corvid_async::task::spawn(async move {
                    let (read, write) = stream.split();
                    let Ok(mut writer) =
                        CompioFrameWriter::start(write, FrameRole::StructuredOnly).await
                    else {
                        return;
                    };
                    let Ok(mut reader) =
                        CompioFrameReader::start(read, FrameRole::StructuredOnly).await
                    else {
                        return;
                    };
                    let Ok(Some(hello)) = reader.next_envelope().await else {
                        return;
                    };
                    let Ok((session, reply)) = accept_hello_with_profiles(
                        &hello,
                        PeerRole::Presentation,
                        &host_str,
                        &epoch,
                        &caps,
                        &[],
                        Some(env!("CARGO_PKG_VERSION")),
                    ) else {
                        return;
                    };
                    if writer.envelope(&reply).await.is_err() {
                        return;
                    }
                    while let Ok(Some(request)) = reader.next_envelope().await {
                        let id = request.request_id.clone();
                        let body = match request.body {
                            Some(Body::GetPresentationSlots(_)) => {
                                Body::PresentationSlots(PresentationSlots {
                                    controller_id: controller.clone(),
                                    connection: state.load(Ordering::Acquire) as i32,
                                    ..Default::default()
                                })
                            }
                            Some(Body::ReconnectAgentMonitor(_)) => {
                                reconnects.fetch_add(1, Ordering::AcqRel);
                                Body::Pong(Pong {})
                            }
                            _ => continue,
                        };
                        let response = Envelope {
                            request_id: id,
                            ..session.envelope(None, body)
                        };
                        if writer.envelope(&response).await.is_err() {
                            break;
                        }
                    }
                });
            }
            Ok::<_, anyhow::Error>(())
        };

        let client = async {
            let unused_exe = Path::new("/nonexistent/corvid");
            scope.refresh_running(unused_exe).await?;
            assert_eq!(reconnect_count.load(Ordering::Acquire), 0);

            scope.ensure_running(unused_exe).await?;
            assert_eq!(reconnect_count.load(Ordering::Acquire), 1);

            connection_state.store(CompanionConnection::Ready as usize, Ordering::Release);
            scope.ensure_running(unused_exe).await?;
            assert_eq!(reconnect_count.load(Ordering::Acquire), 1);
            Ok::<_, anyhow::Error>(())
        };

        corvid_async::select! {
            result = client => result,
            result = server => result,
        }
    }
}
