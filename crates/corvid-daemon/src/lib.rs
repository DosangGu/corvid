mod agents;
mod collector;
mod control;
mod history;
mod identity;
mod incoming;
mod metadata;
mod process;
mod projection;
mod provider;
mod publication;
mod snapshot;
pub mod terminal;
mod tui;
mod workspaces;

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use corvid_protocol::COMMAND_TIMEOUT;
use corvid_protocol::unix::{RuntimeDir, SocketRole};
use tokio::task::JoinSet;

use crate::collector::HookEnvironment;
use crate::identity::Identity;
use crate::publication::MAX_WATCHES;
use crate::workspaces::Workspaces;

// Keep room for ordinary requests when the snapshot watch limit is full.
const MAX_CONTROL_CONNECTIONS: usize = MAX_WATCHES + 8;
const MAX_TUI_CONNECTIONS: usize = 8;
const MAX_COLLECTOR_CONNECTIONS: usize = 32;

pub async fn run(
    runtime_path: &Path,
    state_path: &Path,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let runtime = RuntimeDir::create(runtime_path)?;
    let instance = runtime.lock_instance()?;
    let state = RuntimeDir::create_all(state_path)?;
    let identity = Arc::new(Identity::load(&state)?);
    let (metadata, updates, saver) = metadata::start(state)?;
    let control_socket = instance.bind(SocketRole::Control).await?;
    let client_socket = instance.bind(SocketRole::Client).await?;
    let collector_socket = instance.bind(SocketRole::Collector).await?;
    let hook_environment = Arc::new(HookEnvironment {
        runtime: runtime.path().to_owned(),
        identity: Arc::clone(&identity),
    });
    let workspaces = Arc::new(Workspaces::new(updates, hook_environment));
    let mut controls = JoinSet::new();
    let mut clients = JoinSet::new();
    let mut collectors = JoinSet::new();
    let mut agent_clock = tokio::time::interval(Duration::from_millis(250));
    agent_clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => break,
            _ = controls.join_next(), if !controls.is_empty() => {}
            _ = clients.join_next(), if !clients.is_empty() => {}
            _ = collectors.join_next(), if !collectors.is_empty() => {}
            _ = agent_clock.tick() => workspaces.tick_agents(),
            peer = collector_socket.accept() => match peer {
                Ok(Some(stream)) if collectors.len() < MAX_COLLECTOR_CONNECTIONS => {
                    let identity = Arc::clone(&identity);
                    let workspaces = Arc::clone(&workspaces);
                    collectors.spawn(async move { collector::serve(stream, &identity, &workspaces).await });
                }
                Ok(_) => {}
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
