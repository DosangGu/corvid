use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use corvid_client::ControlEndpoint;
use tokio::sync::watch;
use uuid::Uuid;

use crate::terminal_session::Inventory;

pub(super) fn start(
    endpoint: ControlEndpoint,
    host: String,
) -> Result<watch::Receiver<Option<Arc<Inventory>>>> {
    let (send, receive) = watch::channel(None);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    std::thread::Builder::new()
        .name("corvid-desktop-host-watch".into())
        .spawn(move || {
            runtime.block_on(async {
                tokio::select! {
                    _ = send.closed() => {}
                    _ = monitor(&endpoint, &host, &send) => {}
                }
            });
        })?;
    Ok(receive)
}

async fn monitor(
    endpoint: &ControlEndpoint,
    host: &str,
    publish: &watch::Sender<Option<Arc<Inventory>>>,
) {
    let controller = Uuid::new_v4().to_string();
    loop {
        if let Ok(mut watch) = endpoint.watch(&controller, host, || {}).await {
            loop {
                publish.send_replace(Some(Arc::new(Inventory::read(watch.state()))));
                if watch.next_change().await.is_err() {
                    break;
                }
            }
        }
        publish.send_if_modified(|inventory| inventory.take().is_some());
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}
