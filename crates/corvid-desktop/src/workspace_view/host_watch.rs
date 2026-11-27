use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use corvid_async::{oneshot, watch};
use corvid_client::{ControlEndpoint, daemon_missing, runtime_path};

use crate::inventory_runtime::{self, timeout};
use crate::terminal_session::Inventory;

use super::{Destination, KnownSessions};
use crate::daemon_prompt::has_fault;

pub(super) fn lookup(
    destination: Destination,
    known: Option<(ControlEndpoint, String)>,
) -> Result<oneshot::Receiver<Result<KnownSessions, &'static str>>> {
    let (mut send, receive) = oneshot::channel();
    inventory_runtime::start("corvid-desktop-host-lookup", move || async move {
        let lookup = timeout(Duration::from_secs(30), read_inventory(destination, known));
        corvid_async::select! {
            _ = send.closed() => {}
            result = lookup => {
                let result = match result {
                    Ok(result) => result.map_err(|error| lookup_status(&error)),
                    Err(_) => Err("Lookup timed out"),
                };
                let _ = send.send(result);
            }
        }
    })?;
    Ok(receive)
}

async fn read_inventory(
    destination: Destination,
    known: Option<(ControlEndpoint, String)>,
) -> Result<KnownSessions> {
    let (endpoint, host) = if let Some((endpoint, host)) = known {
        (endpoint, Some(host))
    } else if let Some(remote) = destination.remote() {
        let platform = remote.probe_platform().await?;
        let daemon = remote.probe_daemon(platform).await?.ok_or_else(|| {
            corvid_protocol::ProtocolFault::new("RelayUnavailable", "Corvid is not installed")
        })?;
        (remote.with_daemon(platform, daemon).endpoint(), None)
    } else {
        (ControlEndpoint::Local(runtime_path(None)?), None)
    };
    let state = endpoint.current_snapshot(host.as_deref()).await?;
    Ok(KnownSessions {
        host_id: state.session().host_id.clone(),
        endpoint,
        inventory: Some(Arc::new(Inventory::read(&state))),
        live: true,
        watch: None,
    })
}

fn lookup_status(error: &anyhow::Error) -> &'static str {
    if has_fault(error, "HostKeyRequired") || has_fault(error, "AuthenticationRequired") {
        "SSH authentication needs attention"
    } else if has_fault(error, "HostMismatch") {
        "Machine identity changed"
    } else if has_fault(error, "RelayUnavailable") {
        "Corvid is not installed"
    } else if has_fault(error, "DaemonUnavailable") || daemon_missing(error) {
        "Corvid daemon is not running"
    } else {
        "Could not load workspaces"
    }
}

pub(super) fn start(
    endpoint: ControlEndpoint,
    host: String,
) -> Result<watch::Receiver<Option<Arc<Inventory>>>> {
    let subscription = crate::snapshot_hub::subscribe(endpoint, host)?;
    let (send, receive) = watch::channel(None);
    inventory_runtime::start("corvid-desktop-host-watch", move || async move {
        let mut updates = subscription.receiver();
        send.send_replace(updates.borrow().inventory.clone());
        loop {
            corvid_async::select! {
                _ = send.closed() => return,
                changed = updates.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    send.send_replace(updates.borrow().inventory.clone());
                }
            }
        }
    })?;
    Ok(receive)
}
