use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use corvid_protocol::COMMAND_TIMEOUT;
use corvid_protocol::windows::{DaemonPipe, StateDirectory};

use crate::collector;
use crate::collector::HookEnvironment;
use crate::executor::PeerTasks;
use crate::identity::Identity;
use crate::platform::Listener;
use crate::workspaces::Workspaces;
use crate::{
    MAX_CONTROL_CONNECTIONS, MAX_TUI_CONNECTIONS, control, handoff, metadata, tui, upgrade,
};

pub async fn run(
    runtime_path: &Path,
    state_path: &Path,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    run_with_options(runtime_path, state_path, false, shutdown).await
}

pub async fn run_with_options(
    runtime_path: &Path,
    state_path: &Path,
    autostarted: bool,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    return crate::executor::run(runtime_path, state_path, autostarted, shutdown).await;
}

pub(crate) async fn serve_daemon(
    runtime_path: &Path,
    state_path: &Path,
    autostarted: bool,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let runtime = StateDirectory::create_all(runtime_path)?;
    let _instance = runtime.lock_instance()?;
    let state = StateDirectory::create_all(state_path)?;
    let lock = state.lock_storage()?;
    let identity = Arc::new(Identity::load(&state)?);
    let control_pipe = Listener::bind(&runtime, DaemonPipe::Control)?;
    let client_pipe = Listener::bind(&runtime, DaemonPipe::Client)?;
    let collector_pipe = Listener::bind(&runtime, DaemonPipe::Collector)?;
    let (metadata, updates, saver) = metadata::start(state, lock)?;
    let workspaces = Workspaces::new(
        updates,
        Arc::new(HookEnvironment {
            runtime: runtime_path.to_owned(),
            identity: Arc::clone(&identity),
        }),
    );
    let workspaces = if autostarted {
        workspaces.enable_terminal_hosts(runtime_path)?
    } else {
        workspaces
    };
    let workspaces = Arc::new(workspaces);
    if autostarted {
        handoff::recover(runtime_path, &identity, &workspaces).await?;
    }
    let upgrade_pipe = if autostarted {
        Some(Listener::bind(&runtime, DaemonPipe::Upgrade)?)
    } else {
        None
    };
    let incoming_upgrade = upgrade::listen(upgrade_pipe.as_ref(), &identity);
    corvid_async::pin!(incoming_upgrade);
    let mut controls = PeerTasks::new();
    let mut clients = PeerTasks::new();
    let mut collectors = PeerTasks::new();
    let mut agents = crate::executor::time::interval(Duration::from_secs(1));
    corvid_async::pin!(shutdown);

    // Keep each ConnectNamedPipe operation alive across unrelated events.
    let mut incoming_control = std::pin::pin!(control_pipe.accept());
    let mut incoming_client = std::pin::pin!(client_pipe.accept());
    let mut incoming_collector = std::pin::pin!(collector_pipe.accept());

    loop {
        corvid_async::select! {
            () = &mut shutdown => break,
            _ = controls.join_next() if !controls.is_empty() => {}
            _ = clients.join_next() if !clients.is_empty() => {}
            _ = collectors.join_next() if !collectors.is_empty() => {}
            _ = agents.tick() => workspaces.tick_native_agents().await,
            request = &mut incoming_upgrade => {
                let preserve = request.preserve_terminals;
                let prepare = async {
                    handoff::prepare(runtime_path, &identity, &workspaces, metadata.clone(), &mut clients, preserve).await
                };
                if request.respond(state_path, prepare).await.unwrap_or(false) {
                    break;
                }
                let _ = handoff::cancel(runtime_path, &identity, &workspaces).await;
                incoming_upgrade.set(upgrade::listen(upgrade_pipe.as_ref(), &identity));
            }
            peer = &mut incoming_control => {
                incoming_control.set(control_pipe.accept());
                let stream = peer?;
                if controls.len() < MAX_CONTROL_CONNECTIONS {
                    let identity = Arc::clone(&identity);
                    let metadata = metadata.clone();
                    let workspaces = Arc::clone(&workspaces);
                    controls.spawn(async move {
                        control::serve(stream, &identity, &metadata, &workspaces).await
                    });
                }
            }
            peer = &mut incoming_client => {
                incoming_client.set(client_pipe.accept());
                let stream = peer?;
                if clients.len() < MAX_TUI_CONNECTIONS {
                    let identity = Arc::clone(&identity);
                    let workspaces = Arc::clone(&workspaces);
                    clients.spawn(async move {
                        tui::serve(stream, &identity, &workspaces).await
                    });
                }
            }
            peer = &mut incoming_collector => {
                incoming_collector.set(collector_pipe.accept());
                let stream = peer?;
                if collectors.len() < 32 {
                    let identity = Arc::clone(&identity);
                    let workspaces = Arc::clone(&workspaces);
                    collectors.spawn(async move {
                        collector::serve(stream, &identity, &workspaces).await
                    });
                } else {
                    workspaces.collector.admission_refused();
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
