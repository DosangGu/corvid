use std::collections::BTreeSet;
#[cfg(any(target_os = "linux", windows))]
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_protocol::snapshot::{
    SNAPSHOT_CAPABILITY, SnapshotAssembler, SynchronizedSnapshot, WATCH_CAPABILITY,
};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{Envelope, GetSnapshot, Ping, SnapshotWatch as WatchRequest};
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout};
use uuid::Uuid;

use crate::endpoint::{ControlConnection, ControlEndpoint};

const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
const PING_INTERVAL: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const CACHE_QUEUE: usize = 16;

#[cfg(any(target_os = "linux", windows))]
pub async fn snapshot(runtime: &Path) -> Result<SynchronizedSnapshot> {
    let endpoint = ControlEndpoint::Local(runtime.to_owned());
    let (_, snapshot) = connect(&endpoint, false, true, None, None, || {}).await?;
    Ok(snapshot)
}

#[cfg(any(target_os = "linux", windows))]
pub async fn watch_snapshot(runtime: &Path) -> Result<SnapshotWatch> {
    watch(
        &ControlEndpoint::Local(runtime.to_owned()),
        true,
        None,
        None,
        || {},
    )
    .await
}

impl ControlEndpoint {
    pub async fn view_snapshot(&self, host: &str, epoch: &str) -> Result<SynchronizedSnapshot> {
        let (_, state) = connect(self, false, false, None, Some(host), || {}).await?;
        ensure!(
            state.session().server_epoch == epoch,
            ProtocolFault::new("EpochMismatch", "daemon restarted while restoring the view")
        );
        Ok(state)
    }

    pub async fn watch(
        &self,
        controller: &str,
        expected_host: &str,
        syncing: impl FnOnce(),
    ) -> Result<SnapshotWatch> {
        watch(self, false, Some(controller), Some(expected_host), syncing).await
    }
}

async fn watch(
    endpoint: &ControlEndpoint,
    include_history: bool,
    controller: Option<&str>,
    expected_host: Option<&str>,
    syncing: impl FnOnce(),
) -> Result<SnapshotWatch> {
    let (connection, cache) = connect(
        endpoint,
        true,
        include_history,
        controller,
        expected_host,
        syncing,
    )
    .await?;
    let subscription = cache
        .subscription_id()
        .context("watch subscription missing")?
        .to_owned();
    let (sender, incoming) = mpsc::channel(CACHE_QUEUE);
    let (failed, failure) = watch::channel(None);
    let task = tokio::spawn(async move {
        let error = pump(connection, &subscription, sender).await;
        let fault = error
            .err()
            .map(|error| {
                error
                    .downcast::<ProtocolFault>()
                    .unwrap_or_else(|_| offline())
            })
            .unwrap_or_else(offline);
        failed.send_replace(Some(fault));
    });
    Ok(SnapshotWatch {
        cache,
        incoming,
        failure,
        task,
    })
}

pub struct SnapshotWatch {
    cache: SynchronizedSnapshot,
    incoming: mpsc::Receiver<Envelope>,
    failure: watch::Receiver<Option<ProtocolFault>>,
    task: JoinHandle<()>,
}

impl SnapshotWatch {
    pub fn state(&self) -> &SynchronizedSnapshot {
        &self.cache
    }

    pub fn is_ready(&self) -> bool {
        self.cache.is_valid() && self.failure.borrow().is_none() && !self.task.is_finished()
    }

    /// Wait for the next committed change. Cancellation leaves the frame reader intact.
    pub async fn next_change(&mut self) -> Result<()> {
        loop {
            if let Some(failure) = self.failure.borrow().clone() {
                return Err(failure.into());
            }
            if !self.cache.is_valid() {
                return Err(resync().into());
            }
            let frame = tokio::select! {
                biased;
                _ = self.failure.changed() => {
                    return Err(self.failure.borrow().clone().unwrap_or_else(offline).into());
                }
                frame = self.incoming.recv() => frame.ok_or_else(offline)?,
            };
            match self.cache.apply(frame) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(fault) => {
                    self.task.abort();
                    return Err(fault.into());
                }
            }
        }
    }
}

impl Drop for SnapshotWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn connect(
    endpoint: &ControlEndpoint,
    watch: bool,
    include_history: bool,
    controller: Option<&str>,
    expected_host: Option<&str>,
    syncing: impl FnOnce(),
) -> Result<(ControlConnection, SynchronizedSnapshot)> {
    let capability = if watch {
        WATCH_CAPABILITY
    } else {
        SNAPSHOT_CAPABILITY
    };
    let mut connection = endpoint
        .connect(capability, controller.map(str::to_owned))
        .await?;
    if expected_host.is_some_and(|expected| expected != connection.wire.session.host_id) {
        return Err(ProtocolFault::new("HostMismatch", "daemon host has changed").into());
    }
    syncing();
    let cache = timeout(
        SETUP_TIMEOUT,
        receive_initial(&mut connection, watch, include_history),
    )
    .await
    .context("snapshot setup timed out")??;
    Ok((connection, cache))
}

async fn receive_initial(
    connection: &mut ControlConnection,
    watch: bool,
    include_history: bool,
) -> Result<SynchronizedSnapshot> {
    let connection = &mut connection.wire;
    let request_id = Uuid::new_v4().to_string();
    let mut assembler =
        SnapshotAssembler::new(connection.session.clone(), request_id.clone(), watch);
    let body = if watch {
        Body::SnapshotWatch(WatchRequest { include_history })
    } else {
        Body::GetSnapshot(GetSnapshot { include_history })
    };
    let request = connection.session.envelope(Some(request_id), body);
    timeout(COMMAND_TIMEOUT, connection.writer.envelope(&request)).await??;
    loop {
        let frame = connection
            .reader
            .next_envelope()
            .await?
            .context("daemon closed during snapshot")?;
        if let Some(cache) = assembler.push(frame)? {
            return Ok(cache);
        }
    }
}

async fn pump(
    mut connection: ControlConnection,
    subscription: &str,
    sender: mpsc::Sender<Envelope>,
) -> Result<()> {
    let now = Instant::now();
    let mut heartbeat = Heartbeat {
        pending: BTreeSet::new(),
        next_ping: now + PING_INTERVAL,
        idle: now + IDLE_TIMEOUT,
    };
    loop {
        let frame = heartbeat.read(&mut connection).await?;
        connection.wire.session.validate_scope(&frame)?;
        if let Some(Body::Error(fault)) = &frame.body {
            ensure!(
                frame.subscription_id.as_deref() == Some(subscription)
                    && frame.request_id.is_none(),
                "unexpected watch error scope"
            );
            return Err(ProtocolFault::from_wire(fault).into());
        }
        if matches!(frame.body, Some(Body::Pong(_))) {
            ensure!(
                frame.sequence.is_none()
                    && frame.subscription_id.is_none()
                    && frame
                        .request_id
                        .as_ref()
                        .is_some_and(|id| heartbeat.pending.remove(id)),
                "unexpected watch heartbeat response"
            );
        } else {
            sender.try_send(frame).map_err(|_| resync())?;
        }
        heartbeat.idle = Instant::now() + IDLE_TIMEOUT;
    }
}

struct Heartbeat {
    pending: BTreeSet<String>,
    next_ping: Instant,
    idle: Instant,
}

impl Heartbeat {
    async fn read(&mut self, connection: &mut ControlConnection) -> Result<Envelope> {
        let connection = &mut connection.wire;
        let pending_read = connection.reader.next_envelope();
        tokio::pin!(pending_read);
        loop {
            tokio::select! {
                _ = sleep_until(self.idle) => return Err(offline().into()),
                frame = &mut pending_read => return frame?.context("watch stream closed"),
                _ = sleep_until(self.next_ping) => {
                    if self.pending.len() == 3 {
                        return Err(offline().into());
                    }
                    let id = Uuid::new_v4().to_string();
                    let request = connection.session.envelope(Some(id.clone()), Body::Ping(Ping {}));
                    timeout(COMMAND_TIMEOUT, connection.writer.envelope(&request)).await??;
                    self.pending.insert(id);
                    self.next_ping = Instant::now() + PING_INTERVAL;
                }
            }
        }
    }
}

fn offline() -> ProtocolFault {
    let mut fault = ProtocolFault::new("Offline", "snapshot watch disconnected");
    fault.retryable = true;
    fault
}

fn resync() -> ProtocolFault {
    let mut fault =
        ProtocolFault::new("ResyncRequired", "snapshot watch needs a new full snapshot");
    fault.retryable = true;
    fault
}
