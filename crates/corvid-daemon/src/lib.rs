mod control;
mod identity;
mod metadata;
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

use crate::identity::Identity;
use crate::workspaces::Workspaces;

const MAX_CONTROL_CONNECTIONS: usize = 8;
const MAX_TUI_CONNECTIONS: usize = 8;

pub async fn run(
    runtime_path: &Path,
    state_path: &Path,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let runtime = RuntimeDir::create(runtime_path)?;
    let instance = runtime.lock_instance()?;
    let state = RuntimeDir::create_all(state_path)?;
    let identity = Arc::new(Identity::load(&state)?);
    let (metadata, layouts, saver) = metadata::start(state)?;
    let control_socket = instance.bind(SocketRole::Control).await?;
    let client_socket = instance.bind(SocketRole::Client).await?;
    let workspaces = Arc::new(Workspaces::new(layouts));
    let mut controls = JoinSet::new();
    let mut clients = JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => break,
            _ = controls.join_next(), if !controls.is_empty() => {}
            _ = clients.join_next(), if !clients.is_empty() => {}
            peer = control_socket.accept() => match peer {
                Ok(stream) if controls.len() < MAX_CONTROL_CONNECTIONS => {
                    let identity = Arc::clone(&identity);
                    let metadata = metadata.clone();
                    let workspaces = Arc::clone(&workspaces);
                    controls.spawn(async move { control::serve(stream, &identity, &metadata, &workspaces).await });
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            },
            peer = client_socket.accept() => match peer {
                Ok(stream) if clients.len() < MAX_TUI_CONNECTIONS => {
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
    workspaces.shutdown().await?;
    drop(workspaces);
    tokio::time::timeout(COMMAND_TIMEOUT, saver)
        .await
        .context("layout save shutdown timed out")??;
    Ok(())
}
