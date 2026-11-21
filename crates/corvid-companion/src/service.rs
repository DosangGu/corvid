mod actions;
mod presentation;

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use corvid_async::{mpsc, oneshot};
use corvid_client::{ControlEndpoint, SnapshotWatch, SshRetry};
use corvid_protocol::ProtocolFault;
use corvid_protocol::wire::CompanionConnection;
use corvid_protocol::wire::envelope::Body;
use uuid::Uuid;

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

pub(crate) struct Service {
    store: Store,
    endpoint: ControlEndpoint,
    controller: String,
    host: String,
    connection: CompanionConnection,
    issue: Option<&'static str>,
    generation: Option<Uuid>,
    remote: Option<Remote>,
    save_status_at: Option<Instant>,
    #[cfg(windows)]
    terminals: BTreeMap<Uuid, TerminalRecord>,
    #[cfg(windows)]
    terminal_registry: Option<TerminalRegistry>,
}

impl Service {
    pub(crate) fn new(store: Store, endpoint: ControlEndpoint) -> Self {
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
            connection: CompanionConnection::Connecting,
            issue: None,
            generation: None,
            remote: None,
            save_status_at: None,
            #[cfg(windows)]
            terminals,
            #[cfg(windows)]
            terminal_registry,
        }
    }

    pub(crate) async fn run(&mut self, mut calls: mpsc::Receiver<Call>) -> Result<()> {
        let mut failures = 0;
        let ssh = matches!(self.endpoint, ControlEndpoint::Ssh(_));
        let mut retry = ssh.then(SshRetry::default);
        loop {
            self.connection = CompanionConnection::Connecting;
            let link = match &retry {
                Some(retry) => timeout_at(retry.deadline(), self.connect(&mut calls))
                    .await
                    .unwrap_or_else(|_| Err(SshRetry::exhausted().into())),
                None => self.connect(&mut calls).await,
            };
            let fault = match link {
                Ok(mut watch) => match self.synchronize(&watch, false) {
                    Ok(()) => {
                        failures = 0;
                        retry = None;
                        self.generation = Some(Uuid::new_v4());
                        self.connection = CompanionConnection::Ready;
                        self.issue = None;
                        self.follow(&mut watch, &mut calls).await
                    }
                    Err(fault) => fault.into(),
                },
                Err(error) => error,
            };
            self.generation = None;
            let fault = match self.flush_status() {
                Ok(()) => fault,
                Err(error) => store_fault(error).into(),
            };
            (self.connection, self.issue) = disconnected(&fault);
            let next = if ssh {
                retry
                    .get_or_insert_with(SshRetry::default)
                    .next_attempt(&fault)
            } else {
                Some(Instant::now() + backoff(failures))
            };
            failures = (failures + 1).min(6);
            if let Some(next) = next {
                if !self.wait_to_reconnect(Some(next), &mut calls).await {
                    return Ok(());
                }
                if retry
                    .as_ref()
                    .is_none_or(|retry| Instant::now() < retry.deadline())
                {
                    continue;
                }
            }
            if retry
                .as_ref()
                .is_some_and(|retry| Instant::now() >= retry.deadline())
            {
                self.connection = CompanionConnection::Offline;
                self.issue = Some("SshRetryExhausted");
            }
            if !self.wait_to_reconnect(None, &mut calls).await {
                return Ok(());
            }
            retry = Some(SshRetry::default());
        }
    }

    async fn wait_to_reconnect(
        &mut self,
        next: Option<Instant>,
        calls: &mut mpsc::Receiver<Call>,
    ) -> bool {
        let delay = async {
            match next {
                Some(next) => sleep_until(next).await,
                None => std::future::pending().await,
            }
        };
        corvid_async::pin!(delay);
        loop {
            corvid_async::select! {
                _ = &mut delay => return true,
                call = calls.recv() => {
                    let Some(call) = call else { return false };
                    let reconnect = next.is_none()
                        && !call.reply.is_closed()
                        && matches!(call.body, Body::ReconnectAgentMonitor(_));
                    self.handle(call).await;
                    if reconnect {
                        return true;
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

    async fn connect(&mut self, calls: &mut mpsc::Receiver<Call>) -> Result<SnapshotWatch> {
        if let Some(endpoint) = self.store.monitor_endpoint()? {
            self.endpoint = endpoint;
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
                result = &mut connection => return result,
                started = &mut started if waiting => {
                    waiting = false;
                    if started.is_ok() {
                        self.connection = CompanionConnection::Syncing;
                    }
                }
                call = calls.recv() => {
                    let Some(call) = call else { anyhow::bail!("presentation endpoint closed") };
                    self.handle(call).await;
                }
            }
        }
    }

    async fn follow(
        &mut self,
        watch: &mut SnapshotWatch,
        calls: &mut mpsc::Receiver<Call>,
    ) -> anyhow::Error {
        let now = Instant::now();
        let mut refresh = interval_at(now, Duration::from_secs(1));
        let mut priority_refresh = interval_at(now + PRIORITY_REFRESH, PRIORITY_REFRESH);
        loop {
            corvid_async::select! {
                biased;
                _ = refresh.tick() => {
                    match self.store.monitor_endpoint() {
                        Ok(Some(endpoint)) if endpoint != self.endpoint => {
                            return ProtocolFault::new("ConnectionRequired", "agent monitor connection changed").into();
                        }
                        Err(error) => return error,
                        _ => {}
                    }
                }
                _ = sleep_until(self.save_status_at.unwrap_or_else(Instant::now)) if self.save_status_at.is_some() => {
                    if let Err(error) = self.flush_status() {
                        return store_fault(error).into();
                    }
                }
                _ = priority_refresh.tick() => {
                    let now = now_ms();
                    if let Some(remote) = &self.remote
                        && let Err(error) = self.store.update(|layout| {
                            Ok(layout.reorder(now, |key| priority(remote, key, now))?)
                        })
                    {
                        return store_fault(error).into();
                    }
                }
                update = watch.next_change() => {
                    if let Err(error) = update {
                        return error;
                    }
                    if let Err(fault) = self.synchronize(watch, true) {
                        return fault.into();
                    }
                }
                call = calls.recv() => {
                    let Some(call) = call else { return anyhow::anyhow!("presentation endpoint closed") };
                    if !watch.is_ready() {
                        let fault = ProtocolFault::new("ConnectionRequired", "control watch is unavailable");
                        self.generation = None;
                        (self.connection, self.issue) = disconnected(&anyhow::Error::from(fault.clone()));
                        self.handle(call).await;
                        return fault.into();
                    }
                    if let Some(fault) = self.handle(call).await {
                        return fault.into();
                    }
                }
            }
        }
    }

    fn synchronize(
        &mut self,
        watch: &SnapshotWatch,
        _contiguous: bool,
    ) -> Result<(), ProtocolFault> {
        if !watch.is_ready() {
            return Err(ProtocolFault::new(
                "Offline",
                "control watch is unavailable",
            ));
        }
        let remote = Remote::read(watch.state(), &self.controller)?;
        let removed = self
            .store
            .layout()
            .retired_runs(remote.epoch, &remote.panes);
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
        self.remote = Some(remote);
        self.schedule_status_save();
        Ok(())
    }

    fn schedule_status_save(&mut self) {
        self.save_status_at
            .get_or_insert_with(|| Instant::now() + STATUS_SAVE_DELAY);
    }

    pub(crate) fn flush_status(&mut self) -> Result<()> {
        if self.save_status_at.take().is_some()
            && let Some(remote) = &self.remote
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

fn disconnected(error: &anyhow::Error) -> (CompanionConnection, Option<&'static str>) {
    let (state, issue) = match error
        .downcast_ref::<ProtocolFault>()
        .map(|fault| fault.code.as_str())
    {
        Some("HostMismatch") => (CompanionConnection::Incompatible, "HostMismatch"),
        Some("ProtocolMismatch" | "UnsupportedCapability") => {
            (CompanionConnection::Incompatible, "ProtocolMismatch")
        }
        Some("AuthenticationRequired") => (CompanionConnection::Error, "AuthenticationRequired"),
        Some("HostKeyRequired") => (CompanionConnection::Error, "HostKeyRequired"),
        Some("RelayUnavailable") => (CompanionConnection::Error, "RelayUnavailable"),
        Some("InvalidMessage") => (CompanionConnection::Error, "InvalidMessage"),
        Some("PersistenceFailed") => (CompanionConnection::Error, "PersistenceFailed"),
        Some("LimitExceeded") => (CompanionConnection::Error, "LimitExceeded"),
        Some("DaemonUnavailable") => (CompanionConnection::Offline, "DaemonUnavailable"),
        Some("ResyncRequired") => (CompanionConnection::Syncing, "ResyncRequired"),
        _ => (CompanionConnection::Offline, "ConnectionRequired"),
    };
    (state, Some(issue))
}

fn backoff(failures: u32) -> Duration {
    let base = (500 * (1u64 << failures)).min(20_000);
    let jitter = Uuid::new_v4().as_u128() as u64 % (base / 2);
    Duration::from_millis(base + jitter)
}
