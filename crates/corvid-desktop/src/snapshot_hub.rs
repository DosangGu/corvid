use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use corvid_async::time::Instant;
use corvid_async::{oneshot, watch};
use corvid_client::{ControlEndpoint, SshRetry};
use corvid_protocol::ProtocolFault;
use uuid::Uuid;

use crate::inventory_runtime::{self, sleep, sleep_until, timeout_at};
use crate::terminal_session::Inventory;

const RETRY_MIN: Duration = Duration::from_millis(100);
const RETRY_MAX: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum EndpointKey {
    Local(PathBuf),
    Ssh(String),
    Wsl(String),
}

impl From<&ControlEndpoint> for EndpointKey {
    fn from(endpoint: &ControlEndpoint) -> Self {
        match endpoint {
            ControlEndpoint::Local(path) => Self::Local(path.clone()),
            ControlEndpoint::Ssh(alias) => Self::Ssh(alias.as_str().to_owned()),
            ControlEndpoint::Wsl(distribution) => Self::Wsl(distribution.as_str().to_owned()),
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct SnapshotUpdate {
    pub(crate) epoch: Option<String>,
    pub(crate) inventory: Option<Arc<Inventory>>,
    pub(crate) offline: bool,
}

type RegistryKey = (EndpointKey, String);
type Registry = Mutex<HashMap<RegistryKey, Weak<SharedWatch>>>;

fn registry() -> &'static Registry {
    static HUB: OnceLock<Registry> = OnceLock::new();
    HUB.get_or_init(|| Mutex::new(HashMap::new()))
}

struct SharedWatch {
    key: RegistryKey,
    updates: watch::Sender<SnapshotUpdate>,
    reconnect: watch::Sender<()>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for SharedWatch {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Ok(mut guard) = registry().lock()
            && guard
                .get(&self.key)
                .is_some_and(|weak| weak.strong_count() == 0)
        {
            guard.remove(&self.key);
        }
    }
}

pub(crate) struct Subscription {
    shared: Arc<SharedWatch>,
    updates: watch::Receiver<SnapshotUpdate>,
}

impl Subscription {
    pub(crate) fn receiver(&self) -> watch::Receiver<SnapshotUpdate> {
        self.updates.clone()
    }

    pub(crate) fn reconnect(&self) {
        let _ = self.shared.reconnect.send(());
    }

    #[cfg(test)]
    pub(crate) fn same_stream(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

/// Subscribe to a shared snapshot watch stream for `(endpoint, host)`.
pub(crate) fn subscribe(endpoint: ControlEndpoint, host: String) -> Result<Subscription> {
    let key = (EndpointKey::from(&endpoint), host.clone());
    let mut guard = registry()
        .lock()
        .map_err(|_| anyhow::anyhow!("snapshot hub registry poisoned"))?;
    if let Some(existing) = guard.get(&key).and_then(Weak::upgrade) {
        let updates = existing.updates.subscribe();
        return Ok(Subscription {
            shared: existing,
            updates,
        });
    }

    let (updates, receive_updates) = watch::channel(SnapshotUpdate::default());
    let (reconnect, receive_reconnect) = watch::channel(());
    let (stop, stopped) = oneshot::channel();
    let publish = updates.clone();
    let thread = inventory_runtime::start("corvid-desktop-snapshot-hub", move || async move {
        corvid_async::select! {
            _ = stopped => {}
            _ = monitor(endpoint, host, publish, receive_reconnect) => {}
        }
    })?;
    let shared = Arc::new(SharedWatch {
        key: key.clone(),
        updates,
        reconnect,
        stop: Some(stop),
        thread: Some(thread),
    });
    guard.insert(key, Arc::downgrade(&shared));
    Ok(Subscription {
        shared,
        updates: receive_updates,
    })
}

async fn monitor(
    endpoint: ControlEndpoint,
    host: String,
    updates: watch::Sender<SnapshotUpdate>,
    mut manual: watch::Receiver<()>,
) {
    let controller = Uuid::new_v4().to_string();
    let ssh = matches!(endpoint, ControlEndpoint::Ssh(_));
    let mut ssh_retry = ssh.then(SshRetry::default);
    let mut retry = RETRY_MIN;
    let mut last_failure = None;
    loop {
        let started = Instant::now();
        let error = synchronize(&endpoint, &controller, &host, &updates, &mut ssh_retry)
            .await
            .expect_err("a workspace watch ends only on failure");
        updates.send_replace(SnapshotUpdate {
            epoch: None,
            inventory: None,
            offline: false,
        });

        // Diagnostics contain only a category, never remote error text or titles.
        let reason = match error
            .downcast_ref::<ProtocolFault>()
            .map(|fault| fault.code.as_str())
        {
            Some("ResyncRequired") => "resynchronization required",
            Some("HostMismatch" | "EpochMismatch") => "daemon identity changed",
            Some("LimitExceeded") => "subscription limit reached",
            _ => "connection or protocol failure",
        };
        if last_failure != Some(reason) {
            eprintln!("corvid: workspace synchronization interrupted: {reason}");
            last_failure = Some(reason);
        }
        if ssh {
            let retry = ssh_retry.get_or_insert_with(SshRetry::default);
            if let Some(next) = retry.next_attempt(&error) {
                sleep_until(next).await;
                if Instant::now() < retry.deadline() {
                    continue;
                }
            }
            manual.borrow_and_update();
            updates.send_replace(SnapshotUpdate {
                epoch: None,
                inventory: None,
                offline: true,
            });
            if manual.changed().await.is_err() {
                return;
            }
            updates.send_replace(SnapshotUpdate {
                epoch: None,
                inventory: None,
                offline: false,
            });
            ssh_retry = Some(SshRetry::default());
            continue;
        }
        if started.elapsed() >= RETRY_MAX {
            retry = RETRY_MIN;
        }
        sleep(retry).await;
        retry = (retry * 2).min(RETRY_MAX);
    }
}

async fn synchronize(
    endpoint: &ControlEndpoint,
    controller: &str,
    host: &str,
    updates: &watch::Sender<SnapshotUpdate>,
    retry: &mut Option<SshRetry>,
) -> Result<()> {
    let connection = endpoint.watch(controller, host, || {});
    let mut watch = match retry {
        Some(retry) => timeout_at(retry.deadline(), connection).await??,
        None => connection.await?,
    };
    *retry = None;
    let epoch = watch.state().session().server_epoch.clone();
    loop {
        updates.send_replace(SnapshotUpdate {
            epoch: Some(epoch.clone()),
            inventory: Some(Arc::new(Inventory::read(watch.state()))),
            offline: false,
        });
        watch.next_change().await?;
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use corvid_client::{ControlEndpoint, SshAlias};

    use super::{EndpointKey, subscribe};

    #[test]
    fn endpoint_key_distinguishes_transports() {
        let local = EndpointKey::from(&ControlEndpoint::Local(PathBuf::from("/tmp/a")));
        let ssh = EndpointKey::from(&ControlEndpoint::Ssh(
            SshAlias::new("host-a".into()).unwrap(),
        ));
        assert_ne!(local, ssh);
    }

    #[test]
    fn shares_one_watch_per_endpoint_and_host_until_dropped() {
        let endpoint = ControlEndpoint::Local(PathBuf::from("/tmp/corvid-hub-test-missing"));
        let host = "hub-test-host".to_owned();
        let first = subscribe(endpoint.clone(), host.clone()).unwrap();
        let second = subscribe(endpoint.clone(), host.clone()).unwrap();
        assert!(first.same_stream(&second));

        let other = subscribe(endpoint.clone(), "other-host".to_owned()).unwrap();
        assert!(!first.same_stream(&other));

        drop(first);
        drop(second);

        let third = subscribe(endpoint, host).unwrap();
        assert!(!other.same_stream(&third));
    }
}
