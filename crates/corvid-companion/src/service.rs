mod actions;
mod connection;
mod presentation;

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use corvid_async::{mpsc, oneshot, watch};
use corvid_client::{ControlEndpoint, SnapshotWatch, SshRetry};
use corvid_protocol::ProtocolFault;
use corvid_protocol::wire::envelope::Body;
use uuid::Uuid;

use self::connection::{Connection, ReadySnapshot};
use crate::executor::{Instant, interval_at, sleep_until, timeout_at};
use crate::last_known::LastKnown;
#[cfg(windows)]
use crate::platform::{TerminalRecord, TerminalRegistry};
use crate::priority::{Priority, now_ms};
use crate::remote::Remote;
use crate::{RunKey, RunObservation, Store};

type Outcome = Result<Body, ProtocolFault>;
type Observations = BTreeMap<RunKey, RunObservation>;

const STATUS_SAVE_DELAY: Duration = Duration::from_secs(1);
const PRIORITY_REFRESH: Duration = Duration::from_secs(10);

pub(crate) struct Call {
    pub(crate) body: Body,
    pub(crate) reply: oneshot::Sender<Outcome>,
}

impl Call {
    fn requests_reconnect(&self) -> bool {
        matches!(self.body, Body::ReconnectAgentMonitor(_)) && !self.reply.is_closed()
    }
}

enum Reconnect {
    Retry,
    Requested,
    Closed,
}

enum ConnectOutcome {
    Connected(Box<Result<SnapshotWatch>>),
    ReconnectRequested,
    Closed,
}

pub(crate) struct Service {
    store: Store,
    endpoint: ControlEndpoint,
    controller: String,
    host: String,
    connection: Connection,
    changes: watch::Sender<u64>,
    last_snapshot: Option<Remote>,
    save_status_at: Option<Instant>,
    #[cfg(windows)]
    terminals: BTreeMap<Uuid, TerminalRecord>,
    #[cfg(windows)]
    terminal_registry: Option<TerminalRegistry>,
}

impl Service {
    pub(crate) fn new(
        store: Store,
        endpoint: ControlEndpoint,
        changes: watch::Sender<u64>,
    ) -> Self {
        let config = store.config();
        #[cfg(windows)]
        let (terminal_registry, terminals) =
            match TerminalRegistry::new(config.controller_id, config.host_id)
                .and_then(|registry| Ok((registry.load()?, registry)))
            {
                Ok((records, registry)) => (Some(registry), records),
                Err(_) => {
                    eprintln!("corvid companion: saved WezTerm registrations unavailable");
                    (None, BTreeMap::new())
                }
            };
        Self {
            endpoint,
            controller: config.controller_id.to_string(),
            host: config.host_id.to_string(),
            store,
            connection: Connection::Connecting(None),
            changes,
            last_snapshot: None,
            save_status_at: None,
            #[cfg(windows)]
            terminals,
            #[cfg(windows)]
            terminal_registry,
        }
    }

    pub(super) fn notify_change(&self) {
        self.changes
            .send_modify(|revision| *revision = revision.saturating_add(1));
    }

    pub(super) fn state_revision(&self) -> u64 {
        *self.changes.borrow()
    }

    fn set_connection(&mut self, next: Connection) {
        self.connection = next;
        self.notify_change();
    }

    pub(crate) async fn run(&mut self, mut calls: mpsc::Receiver<Call>) -> Result<()> {
        let mut failures = 0;
        let ssh = matches!(self.endpoint, ControlEndpoint::Ssh(_));
        let mut retry = ssh.then(SshRetry::default);
        loop {
            self.set_connection(Connection::Connecting(self.connection.issue()));
            let link = match &retry {
                Some(retry) => timeout_at(retry.deadline(), self.connect(&mut calls))
                    .await
                    .unwrap_or_else(|_| {
                        ConnectOutcome::Connected(Box::new(Err(SshRetry::exhausted().into())))
                    }),
                None => self.connect(&mut calls).await,
            };
            let link = match link {
                ConnectOutcome::Connected(result) => *result,
                ConnectOutcome::ReconnectRequested => {
                    failures = 0;
                    retry = ssh.then(SshRetry::default);
                    continue;
                }
                ConnectOutcome::Closed => return Ok(()),
            };
            let result = match link {
                Ok(mut watch) => match self.synchronize(&watch) {
                    Ok(remote) => {
                        failures = 0;
                        retry = None;
                        self.publish_ready(remote);
                        self.follow(&mut watch, &mut calls).await
                    }
                    Err(fault) => Err(fault.into()),
                },
                Err(error) => Err(error),
            };
            self.retire_snapshot(Connection::Connecting(None));
            let fault = match self
                .flush_status()
                .map_err(|error| store_fault(error).into())
                .and(result)
            {
                Ok(()) => {
                    failures = 0;
                    retry = ssh.then(SshRetry::default);
                    continue;
                }
                Err(error) => error,
            };
            self.set_connection(Connection::disconnected(&fault));
            let next = if ssh {
                retry
                    .get_or_insert_with(SshRetry::default)
                    .next_attempt(&fault)
            } else {
                Some(Instant::now() + backoff(failures))
            };
            failures = (failures + 1).min(6);
            if let Some(next) = next {
                match self.wait_to_reconnect(Some(next), &mut calls).await {
                    Reconnect::Requested => {
                        failures = 0;
                        retry = ssh.then(SshRetry::default);
                        continue;
                    }
                    Reconnect::Retry
                        if retry
                            .as_ref()
                            .is_none_or(|retry| Instant::now() < retry.deadline()) =>
                    {
                        continue;
                    }
                    Reconnect::Retry => {}
                    Reconnect::Closed => return Ok(()),
                }
            }
            if retry
                .as_ref()
                .is_some_and(|retry| Instant::now() >= retry.deadline())
            {
                self.set_connection(Connection::Offline("SshRetryExhausted"));
            }
            if matches!(
                self.wait_to_reconnect(None, &mut calls).await,
                Reconnect::Closed
            ) {
                return Ok(());
            }
            failures = 0;
            retry = ssh.then(SshRetry::default);
        }
    }

    fn publish_ready(&mut self, remote: Remote) {
        self.last_snapshot = None;
        self.set_connection(Connection::Ready(ReadySnapshot {
            generation: Uuid::new_v4(),
            remote,
        }));
    }

    fn retire_snapshot(&mut self, connection: Connection) {
        if let Connection::Ready(snapshot) = std::mem::replace(&mut self.connection, connection) {
            self.last_snapshot = Some(snapshot.remote);
        }
        self.notify_change();
    }

    fn latest_snapshot(&self) -> Option<&Remote> {
        self.connection
            .ready()
            .map(|snapshot| &snapshot.remote)
            .or(self.last_snapshot.as_ref())
    }

    async fn wait_to_reconnect(
        &mut self,
        next: Option<Instant>,
        calls: &mut mpsc::Receiver<Call>,
    ) -> Reconnect {
        let delay = async {
            match next {
                Some(next) => sleep_until(next).await,
                None => std::future::pending().await,
            }
        };
        corvid_async::pin!(delay);
        loop {
            corvid_async::select! {
                _ = &mut delay => return Reconnect::Retry,
                call = calls.recv() => {
                    let Some(call) = call else { return Reconnect::Closed };
                    let reconnect = call.requests_reconnect();
                    self.handle(call).await;
                    if reconnect {
                        return Reconnect::Requested;
                    }
                }
            }
        }
    }

    #[cfg(windows)]
    fn local_registration(
        &self,
        remote: &Remote,
        client: Uuid,
        revision: u64,
    ) -> Option<&TerminalRecord> {
        self.terminals
            .get(&client)
            .filter(|terminal| terminal.matches(remote.epoch, revision, remote.sequence))
    }

    #[cfg(windows)]
    fn save_terminals(&self) {
        if let Some(registry) = &self.terminal_registry
            && registry.save(&self.terminals).is_err()
        {
            eprintln!("corvid companion: could not save WezTerm registrations");
        }
    }

    #[cfg(windows)]
    fn reconcile_terminals(&mut self, remote: &Remote) {
        let before = self.terminals.len();
        self.terminals.retain(|client, terminal| {
            terminal.epoch == remote.epoch
                && (remote.sequence < terminal.commit_sequence
                    || remote.bindings.get(client) == Some(&terminal.revision))
        });
        if self.terminals.len() != before {
            self.save_terminals();
        }
    }

    async fn connect(&mut self, calls: &mut mpsc::Receiver<Call>) -> ConnectOutcome {
        match self.store.monitor_endpoint() {
            Ok(Some(endpoint)) => self.endpoint = endpoint,
            Ok(None) => {}
            Err(error) => return ConnectOutcome::Connected(Box::new(Err(error))),
        }
        let (syncing, mut started) = oneshot::channel();
        let endpoint = self.endpoint.clone();
        let controller = self.controller.clone();
        let host = self.host.clone();
        let connection = endpoint.watch(&controller, &host, move || {
            let _ = syncing.send(());
        });
        corvid_async::pin!(connection);
        let mut waiting = true;
        loop {
            corvid_async::select! {
                result = &mut connection => return ConnectOutcome::Connected(Box::new(result)),
                started = &mut started if waiting => {
                    waiting = false;
                    if started.is_ok() {
                        self.set_connection(Connection::Syncing(self.connection.issue()));
                    }
                }
                call = calls.recv() => {
                    let Some(call) = call else { return ConnectOutcome::Closed };
                    let reconnect = call.requests_reconnect();
                    self.handle(call).await;
                    if reconnect {
                        return ConnectOutcome::ReconnectRequested;
                    }
                }
            }
        }
    }

    async fn follow(
        &mut self,
        watch: &mut SnapshotWatch,
        calls: &mut mpsc::Receiver<Call>,
    ) -> Result<()> {
        let now = Instant::now();
        let mut refresh = interval_at(now, Duration::from_secs(1));
        let mut priority_refresh = interval_at(now + PRIORITY_REFRESH, PRIORITY_REFRESH);
        loop {
            corvid_async::select! {
                biased;
                _ = refresh.tick() => {
                    match self.store.monitor_endpoint() {
                        Ok(Some(endpoint)) if endpoint != self.endpoint => {
                            return Err(ProtocolFault::new(
                                "ConnectionRequired", "agent monitor connection changed"
                            ).into());
                        }
                        Err(error) => return Err(error),
                        _ => {}
                    }
                }
                _ = sleep_until(self.save_status_at.unwrap_or_else(Instant::now)) if self.save_status_at.is_some() => {
                    if let Err(error) = self.flush_status() {
                        return Err(store_fault(error).into());
                    }
                }
                _ = priority_refresh.tick() => {
                    let now = now_ms();
                    let before = self.store.layout_revision();
                    if let Some(snapshot) = self.connection.ready()
                        && let Err(error) = self.store.update(|layout| {
                            Ok(layout.reorder(now, |key| priority(&snapshot.remote, key, now))?)
                        })
                    {
                        return Err(store_fault(error).into());
                    }
                    if self.store.layout_revision() != before {
                        self.notify_change();
                    }
                }
                update = watch.next_change() => {
                    update?;
                    let remote = self.synchronize(watch)?;
                    let Connection::Ready(snapshot) = &mut self.connection else {
                        unreachable!("following a synchronized watch");
                    };
                    snapshot.remote = remote;
                    self.notify_change();
                }
                call = calls.recv() => {
                    let Some(call) = call else { anyhow::bail!("presentation endpoint closed") };
                    if call.requests_reconnect() {
                        self.handle(call).await;
                        return Ok(());
                    }
                    if !watch.is_ready() {
                        let fault = ProtocolFault::new("ConnectionRequired", "control watch is unavailable");
                        self.retire_snapshot(Connection::Offline("ConnectionRequired"));
                        self.handle(call).await;
                        return Err(fault.into());
                    }
                    if let Some(fault) = self.handle(call).await {
                        return Err(fault.into());
                    }
                }
            }
        }
    }

    fn synchronize(&mut self, watch: &SnapshotWatch) -> Result<Remote, ProtocolFault> {
        if !watch.is_ready() {
            return Err(ProtocolFault::new(
                "Offline",
                "control watch is unavailable",
            ));
        }
        let remote = Remote::read(watch.state(), &self.controller)?;
        let mut removed = self
            .store
            .layout()
            .retired_runs(remote.epoch, &remote.panes);
        removed.extend(
            remote
                .runs
                .iter()
                .filter_map(|(key, run)| run.process_exited.then_some(*key)),
        );
        let runs = observations(&remote);
        let now = now_ms();
        self.store
            .update(|layout| {
                Ok(layout.reconcile_prioritized(&runs, &removed, now, |key| {
                    priority(&remote, key, now)
                })?)
            })
            .map_err(store_fault)?;
        #[cfg(windows)]
        self.reconcile_terminals(&remote);
        self.schedule_status_save();
        Ok(remote)
    }

    fn schedule_status_save(&mut self) {
        self.save_status_at
            .get_or_insert_with(|| Instant::now() + STATUS_SAVE_DELAY);
    }

    pub(crate) fn flush_status(&mut self) -> Result<()> {
        if self.save_status_at.take().is_some()
            && let Some(remote) = self
                .connection
                .ready()
                .map(|snapshot| &snapshot.remote)
                .or(self.last_snapshot.as_ref())
        {
            self.store.save_status(|key| {
                remote
                    .runs
                    .get(&key)
                    .map(|run| LastKnown::from_display(&run.display))
            })?;
        }
        Ok(())
    }

    async fn handle(&mut self, call: Call) -> Option<ProtocolFault> {
        if call.reply.is_closed() {
            return None;
        }
        let outcome = self.execute(call.body).await;
        let reconnect = outcome
            .as_ref()
            .err()
            .filter(|fault| {
                matches!(
                    fault.code.as_str(),
                    "EpochMismatch"
                        | "HostMismatch"
                        | "DaemonUnavailable"
                        | "Offline"
                        | "AuthenticationRequired"
                        | "HostKeyRequired"
                )
            })
            .cloned();
        let _ = call.reply.send(outcome);
        reconnect
    }
}

fn priority(remote: &Remote, key: &RunKey, now: u64) -> Priority {
    remote.runs.get(key).map_or_else(Priority::default, |run| {
        Priority::for_run(&run.display, true, now)
    })
}

fn observations(remote: &Remote) -> Observations {
    remote
        .runs
        .iter()
        .map(|(key, run)| (*key, run.observation))
        .collect()
}

fn store_fault(error: anyhow::Error) -> ProtocolFault {
    error.downcast::<ProtocolFault>().unwrap_or_else(|_| {
        ProtocolFault::new("PersistenceFailed", "companion could not save slot state")
    })
}

fn backoff(failures: u32) -> Duration {
    let base = (500 * (1u64 << failures)).min(20_000);
    let jitter = Uuid::new_v4().as_u128() as u64 % (base / 2);
    Duration::from_millis(base + jitter)
}
