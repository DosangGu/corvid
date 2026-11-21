use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use corvid_async::time::Instant;
use corvid_async::{oneshot, watch};
use corvid_client::{ControlEndpoint, SshRetry, daemon_missing, runtime_path};
use uuid::Uuid;

use crate::inventory_runtime::{self, sleep, sleep_until, timeout, timeout_at};
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
    let (send, receive) = watch::channel(None);
    inventory_runtime::start("corvid-desktop-host-watch", move || async move {
        corvid_async::select! {
            _ = send.closed() => {}
            _ = monitor(&endpoint, &host, &send) => {}
        }
    })?;
    Ok(receive)
}

async fn monitor(
    endpoint: &ControlEndpoint,
    host: &str,
    publish: &watch::Sender<Option<Arc<Inventory>>>,
) {
    let controller = Uuid::new_v4().to_string();
    let ssh = matches!(endpoint, ControlEndpoint::Ssh(_));
    let mut retry = ssh.then(SshRetry::default);
    loop {
        let connect = endpoint.watch(&controller, host, || {});
        let result = match &retry {
            Some(retry) => timeout_at(retry.deadline(), connect)
                .await
                .unwrap_or_else(|_| Err(SshRetry::exhausted().into())),
            None => connect.await,
        };
        let error = match result {
            Ok(mut watch) => {
                retry = None;
                loop {
                    publish.send_replace(Some(Arc::new(Inventory::read(watch.state()))));
                    if let Err(error) = watch.next_change().await {
                        break error;
                    }
                }
            }
            Err(error) => error,
        };
        publish.send_replace(None);
        if ssh {
            let retry = retry.get_or_insert_with(SshRetry::default);
            let Some(next) = retry.next_attempt(&error) else {
                return;
            };
            sleep_until(next).await;
            if Instant::now() >= retry.deadline() {
                return;
            }
        } else {
            sleep(Duration::from_secs(3)).await;
        }
    }
}
