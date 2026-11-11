#[cfg_attr(windows, path = "agents/windows.rs")]
#[cfg(any(windows, target_os = "linux"))]
mod agents;
mod collector;
mod control;
mod handoff;
mod history;
mod identity;
mod incoming;
mod metadata;
#[cfg(windows)]
mod native_process;
mod platform;
#[cfg(target_os = "linux")]
mod process;
mod projection;
#[cfg(target_os = "linux")]
mod provider;
mod publication;
mod snapshot;
pub mod terminal;
mod tui;
mod upgrade;
#[cfg(windows)]
mod windows;
mod workspaces;

#[cfg(target_os = "linux")]
use std::future::Future;
#[cfg(target_os = "linux")]
use std::path::Path;
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use anyhow::{Context, Result};
#[cfg(target_os = "linux")]
use corvid_protocol::COMMAND_TIMEOUT;
#[cfg(target_os = "linux")]
use corvid_protocol::unix::{RuntimeDir, SocketRole};
#[cfg(target_os = "linux")]
use tokio::task::JoinSet;

#[cfg(target_os = "linux")]
use crate::collector::HookEnvironment;
#[cfg(target_os = "linux")]
use crate::identity::Identity;
use crate::publication::MAX_WATCHES;
#[cfg(target_os = "linux")]
use crate::workspaces::Workspaces;

#[cfg(target_os = "linux")]
pub use provider::hooks::observer_settings;
#[cfg(target_os = "linux")]
pub use provider::shell as shell_agent;
#[cfg(windows)]
pub use windows::run;
#[cfg(windows)]
pub use windows::run_with_options;

/// Read an existing daemon identity without creating state.
#[cfg(any(windows, target_os = "linux"))]
pub fn stored_host_id(state_path: &std::path::Path) -> anyhow::Result<Option<uuid::Uuid>> {
    identity::read(&platform::StateDirectory::open(state_path)?)
}

// Keep room for ordinary requests when the snapshot watch limit is full.
const MAX_CONTROL_CONNECTIONS: usize = MAX_WATCHES + 8;
const MAX_TUI_CONNECTIONS: usize = 8;
#[cfg(target_os = "linux")]
const MAX_COLLECTOR_CONNECTIONS: usize = 32;

#[cfg(target_os = "linux")]
pub async fn run(
    runtime_path: &Path,
    state_path: &Path,
    autostarted: bool,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let runtime = RuntimeDir::create(runtime_path)?;
    let instance = runtime.lock_instance()?;
    let state = RuntimeDir::create_all(state_path)?;
    let identity = Arc::new(Identity::load(&state)?);
    let lock = state.lock_storage()?;
    let (metadata, updates, saver) = metadata::start(state, lock)?;
    let control_socket = instance.bind(SocketRole::Control).await?;
    let client_socket = instance.bind(SocketRole::Client).await?;
    let collector_socket = instance.bind(SocketRole::Collector).await?;
    let upgrade_socket = if autostarted {
        Some(instance.bind(SocketRole::Upgrade).await?)
    } else {
        None
    };
    let hook_environment = Arc::new(HookEnvironment {
        runtime: runtime.path().to_owned(),
        identity: Arc::clone(&identity),
        shell_commands: shell_agent::Commands::install(runtime.path()).ok(),
    });
    let workspaces = Workspaces::new(updates, hook_environment);
    let workspaces = if autostarted {
        workspaces.enable_terminal_hosts(runtime.path())?
    } else {
        workspaces
    };
    let workspaces = Arc::new(workspaces);
    if autostarted {
        handoff::recover(runtime.path(), &identity, &workspaces).await?;
    }
    let mut controls = JoinSet::new();
    let mut clients = JoinSet::new();
    let mut collectors = JoinSet::new();
    let mut agent_clock = tokio::time::interval(Duration::from_millis(250));
    agent_clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let incoming_upgrade = upgrade::listen(upgrade_socket.as_ref(), &identity);
    tokio::pin!(incoming_upgrade);
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => break,
            _ = controls.join_next(), if !controls.is_empty() => {}
            _ = clients.join_next(), if !clients.is_empty() => {}
            _ = collectors.join_next(), if !collectors.is_empty() => {}
            request = &mut incoming_upgrade => {
                let preserve = request.preserve_terminals;
                let prepare = async {
                    handoff::prepare(runtime.path(), &identity, &workspaces, metadata.clone(), &mut clients, preserve).await
                };
                if request.respond(state_path, prepare).await.unwrap_or(false) {
                    break;
                }
                let _ = handoff::cancel(runtime.path(), &identity, &workspaces).await;
                incoming_upgrade.set(upgrade::listen(upgrade_socket.as_ref(), &identity));
            }
            _ = agent_clock.tick() => workspaces.tick_agents(),
            peer = collector_socket.accept() => match peer {
                Ok(Some(stream)) if collectors.len() < MAX_COLLECTOR_CONNECTIONS => {
                    let identity = Arc::clone(&identity);
                    let workspaces = Arc::clone(&workspaces);
                    collectors.spawn(async move { collector::serve(stream, &identity, &workspaces).await });
                }
                Ok(Some(_)) => workspaces.collector.admission_refused(),
                Ok(None) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            },
            peer = control_socket.accept() => match peer {
                Ok(Some(stream)) if controls.len() < MAX_CONTROL_CONNECTIONS => {
                    let identity = Arc::clone(&identity);
                    let metadata = metadata.clone();
                    let workspaces = Arc::clone(&workspaces);
                    controls.spawn(async move { control::serve(stream, &identity, &metadata, &workspaces).await });
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            },
            peer = client_socket.accept() => match peer {
                Ok(Some(stream)) if clients.len() < MAX_TUI_CONNECTIONS => {
                    let identity = Arc::clone(&identity);
                    let workspaces = Arc::clone(&workspaces);
                    clients.spawn(async move { tui::serve(stream, &identity, &workspaces).await });
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    }
    controls.shutdown().await;
    clients.shutdown().await;
    collectors.shutdown().await;
    workspaces.shutdown().await?;
    drop(workspaces);
    tokio::time::timeout(COMMAND_TIMEOUT, saver)
        .await
        .context("metadata save shutdown timed out")??;
    Ok(())
}
