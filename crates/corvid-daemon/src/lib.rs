#[cfg_attr(windows, path = "agents/windows.rs")]
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
mod agents;
mod collector;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub mod command;
mod control;
mod directories;
mod executor;
mod handoff;
mod history;
mod hook_data;
mod identity;
mod incoming;
mod metadata;
#[cfg(windows)]
mod native_process;
mod platform;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod process;
mod projection;
mod provider;
mod publication;
mod snapshot;
pub mod terminal;
mod tui;
mod upgrade;
#[cfg(windows)]
mod windows;
mod workspaces;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::future::Future;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::path::Path;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::Arc;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::Duration;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::executor::PeerTasks;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use anyhow::{Context, Result};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use corvid_protocol::COMMAND_TIMEOUT;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use corvid_protocol::unix::{RuntimeDir, SocketRole};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::collector::HookEnvironment;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::identity::Identity;
use crate::publication::MAX_WATCHES;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::workspaces::Workspaces;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use provider::hooks::observer_settings;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub use provider::hooks_install::{install_hooks, uninstall_hooks};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use provider::shell as shell_agent;
#[cfg(windows)]
pub use provider::windows_hooks::observer_settings;
#[cfg(windows)]
pub use windows::run;
#[cfg(windows)]
pub use windows::run_with_options;
#[cfg(windows)]
use windows::serve_daemon;

/// Run the daemon on the current Compio runtime. The caller owns shutdown signals.
pub async fn run_native(
    runtime_path: &std::path::Path,
    state_path: &std::path::Path,
    autostarted: bool,
    shutdown: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()> {
    serve_daemon(runtime_path, state_path, autostarted, shutdown).await
}

/// Read an existing daemon identity without creating state.
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
pub fn stored_host_id(state_path: &std::path::Path) -> anyhow::Result<Option<uuid::Uuid>> {
    corvid_client::daemon_control::identity::stored_host_id(state_path)
}

// Keep room for ordinary requests when the snapshot watch limit is full.
const MAX_CONTROL_CONNECTIONS: usize = MAX_WATCHES + 8;
const MAX_TUI_CONNECTIONS: usize = 8;
#[cfg(any(target_os = "linux", target_os = "macos"))]
const MAX_COLLECTOR_CONNECTIONS: usize = 32;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub async fn run(
    runtime_path: &Path,
    state_path: &Path,
    autostarted: bool,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    return executor::run(runtime_path, state_path, autostarted, shutdown).await;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn serve_daemon(
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
    let control_socket = platform::bind(&instance, SocketRole::Control).await?;
    let client_socket = platform::bind(&instance, SocketRole::Client).await?;
    let collector_socket = platform::bind(&instance, SocketRole::Collector).await?;
    let upgrade_socket = if autostarted {
        Some(platform::bind(&instance, SocketRole::Upgrade).await?)
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
    let mut controls = PeerTasks::new();
    let mut clients = PeerTasks::new();
    let mut collectors = PeerTasks::new();
    let mut agent_clock = crate::executor::time::interval(Duration::from_millis(250));
    let incoming_upgrade = upgrade::listen(upgrade_socket.as_ref(), &identity);
    corvid_async::pin!(incoming_upgrade);
    corvid_async::pin!(shutdown);

    // Keep each accept alive across unrelated events. Dropping a completion
    // operation can discard the peer it has already accepted.
    let mut incoming_control = std::pin::pin!(control_socket.accept());
    let mut incoming_client = std::pin::pin!(client_socket.accept());
    let mut incoming_collector = std::pin::pin!(collector_socket.accept());

    loop {
        corvid_async::select! {
            () = &mut shutdown => break,
            _ = controls.join_next() if !controls.is_empty() => {}
            _ = clients.join_next() if !clients.is_empty() => {}
            _ = collectors.join_next() if !collectors.is_empty() => {}
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
            _ = agent_clock.tick() => workspaces.tick_agents().await,
            peer = &mut incoming_collector => {
                incoming_collector.set(collector_socket.accept());
                match peer {
                    Ok(Some(stream)) if collectors.len() < MAX_COLLECTOR_CONNECTIONS => {
                        let identity = Arc::clone(&identity);
                        let workspaces = Arc::clone(&workspaces);
                        collectors.spawn(async move { collector::serve(stream, &identity, &workspaces).await });
                    }
                    Ok(Some(_)) => workspaces.collector.admission_refused(),
                    Ok(None) => {}
                    Err(_) => crate::executor::time::sleep(Duration::from_millis(25)).await,
                }
            },
            peer = &mut incoming_control => {
                incoming_control.set(control_socket.accept());
                match peer {
                    Ok(Some(stream)) if controls.len() < MAX_CONTROL_CONNECTIONS => {
                        let identity = Arc::clone(&identity);
                        let metadata = metadata.clone();
                        let workspaces = Arc::clone(&workspaces);
                        controls.spawn(async move { control::serve(stream, &identity, &metadata, &workspaces).await });
                    }
                    Ok(_) => {}
                    Err(_) => crate::executor::time::sleep(Duration::from_millis(25)).await,
                }
            },
            peer = &mut incoming_client => {
                incoming_client.set(client_socket.accept());
                match peer {
                    Ok(Some(stream)) if clients.len() < MAX_TUI_CONNECTIONS => {
                        let identity = Arc::clone(&identity);
                        let workspaces = Arc::clone(&workspaces);
                        clients.spawn(async move { tui::serve(stream, &identity, &workspaces).await });
                    }
                    Ok(_) => {}
                    Err(_) => crate::executor::time::sleep(Duration::from_millis(25)).await,
                }
            }
        }
    }
    controls.shutdown().await;
    clients.shutdown().await;
    collectors.shutdown().await;
    workspaces.shutdown().await?;
    drop(workspaces);
    crate::executor::time::timeout(COMMAND_TIMEOUT, saver.finish())
        .await
        .context("metadata save shutdown timed out")??;
    Ok(())
}
