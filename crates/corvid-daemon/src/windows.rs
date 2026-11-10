use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use corvid_protocol::COMMAND_TIMEOUT;
use corvid_protocol::windows::{DaemonPipe, StateDirectory};
use tokio::task::JoinSet;

use crate::collector::HookEnvironment;
use crate::identity::Identity;
use crate::workspaces::Workspaces;
use crate::{MAX_CONTROL_CONNECTIONS, MAX_TUI_CONNECTIONS, control, metadata, tui};

pub async fn run(
    runtime_path: &Path,
    state_path: &Path,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let runtime = StateDirectory::create_all(runtime_path)?;
    let _instance = runtime.lock_instance()?;
    let state = StateDirectory::create_all(state_path)?;
    let lock = state.lock_storage()?;
    let identity = Arc::new(Identity::load(&state)?);
    let mut control_pipe = DaemonPipe::Control.bind(&runtime)?;
    let mut client_pipe = DaemonPipe::Client.bind(&runtime)?;
    let (metadata, updates, saver) = metadata::start(state, lock)?;
    let workspaces = Arc::new(Workspaces::new(updates, Arc::new(HookEnvironment {})));
    let mut controls = JoinSet::new();
    let mut clients = JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => break,
            _ = controls.join_next(), if !controls.is_empty() => {}
            _ = clients.join_next(), if !clients.is_empty() => {}
            peer = control_pipe.accept() => {
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
            peer = client_pipe.accept() => {
                let stream = peer?;
                if clients.len() < MAX_TUI_CONNECTIONS {
                    let identity = Arc::clone(&identity);
                    let workspaces = Arc::clone(&workspaces);
                    clients.spawn(async move {
                        tui::serve(stream, &identity, &workspaces).await
                    });
                }
            }
        }
    }
    controls.shutdown().await;
    clients.shutdown().await;
    workspaces.shutdown().await?;
    drop(workspaces);
    tokio::time::timeout(COMMAND_TIMEOUT, saver)
        .await
        .context("metadata save shutdown timed out")??;
    Ok(())
}
