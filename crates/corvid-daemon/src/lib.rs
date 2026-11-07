mod control;
mod identity;

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use corvid_protocol::unix::{RuntimeDir, SocketRole};
use tokio::task::JoinSet;

use crate::identity::Identity;

const MAX_CONTROL_CONNECTIONS: usize = 8;

pub async fn run(
    runtime_path: &Path,
    state_path: &Path,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let runtime = RuntimeDir::create(runtime_path)?;
    let instance = runtime.lock_instance()?;
    let state = RuntimeDir::create_all(state_path)?;
    let identity = Arc::new(Identity::load(&state)?);
    let control_socket = instance.bind(SocketRole::Control).await?;
    let mut controls = JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => break,
            _ = controls.join_next(), if !controls.is_empty() => {}
            peer = control_socket.accept() => match peer {
                Ok(stream) if controls.len() < MAX_CONTROL_CONNECTIONS => {
                    let identity = Arc::clone(&identity);
                    controls.spawn(async move { control::serve(stream, &identity).await });
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    }
    controls.shutdown().await;
    Ok(())
}
