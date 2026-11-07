mod actions;
mod presentation;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::Result;
use corvid_client::{ControlEndpoint, SnapshotWatch};
use corvid_protocol::ProtocolFault;
use corvid_protocol::wire::CompanionConnection;
use corvid_protocol::wire::envelope::Body;
use tokio::sync::{mpsc, oneshot};
use tokio::time::sleep;
use uuid::Uuid;

#[cfg(windows)]
use crate::platform::WezTermPane;
use crate::remote::Remote;
use crate::{RunKey, RunObservation, Store};

type Outcome = Result<Body, ProtocolFault>;
type Observations = BTreeMap<RunKey, RunObservation>;

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
    #[cfg(windows)]
    terminals: BTreeMap<Uuid, LocalTerminal>,
}

#[cfg(windows)]
struct LocalTerminal {
    epoch: Uuid,
    revision: u64,
    commit_sequence: u64,
    pane: WezTermPane,
}

impl Service {
    pub(crate) fn new(store: Store) -> Result<Self> {
        let config = store.config();
        Ok(Self {
            endpoint: config.control_endpoint()?,
            controller: config.controller_id.to_string(),
            host: config.host_id.to_string(),
            store,
            connection: CompanionConnection::Connecting,
            issue: None,
            generation: None,
            remote: None,
            #[cfg(windows)]
            terminals: BTreeMap::new(),
        })
    }

    pub(crate) async fn run(mut self, mut calls: mpsc::Receiver<Call>) -> Result<()> {
        let mut failures = 0;
        loop {
            self.connection = CompanionConnection::Connecting;
            let fault = match self.connect(&mut calls).await {
                Ok(mut watch) => match self.synchronize(&watch, false) {
                    Ok(()) => {
                        failures = 0;
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
            (self.connection, self.issue) = disconnected(&fault);
            let delay = sleep(backoff(failures));
            failures = (failures + 1).min(6);
            tokio::pin!(delay);
            loop {
                tokio::select! {
                    _ = &mut delay => break,
                    call = calls.recv() => {
                        let Some(call) = call else { return Ok(()) };
                        self.handle(call).await;
                    }
                }
            }
        }
    }

    #[cfg(windows)]
    fn local_pane(&self, remote: &Remote, client: Uuid, revision: u64) -> Option<&WezTermPane> {
        self.terminals
            .get(&client)
            .filter(|terminal| terminal.epoch == remote.epoch && terminal.revision == revision)
            .map(|terminal| &terminal.pane)
    }

    async fn connect(&mut self, calls: &mut mpsc::Receiver<Call>) -> Result<SnapshotWatch> {
        let (syncing, mut started) = oneshot::channel();
        let endpoint = self.endpoint.clone();
        let controller = self.controller.clone();
        let host = self.host.clone();
        let connection = endpoint.watch(&controller, &host, move || {
            let _ = syncing.send(());
        });
        tokio::pin!(connection);
        let mut waiting = true;
        loop {
            tokio::select! {
                result = &mut connection => return result,
                started = &mut started, if waiting => {
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
        loop {
            tokio::select! {
                biased;
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
        contiguous: bool,
    ) -> Result<(), ProtocolFault> {
        if !watch.is_ready() {
            return Err(ProtocolFault::new(
                "Offline",
                "control watch is unavailable",
            ));
        }
        let remote = Remote::read(watch.state(), &self.controller)?;
        let removed = self
            .remote
            .as_ref()
            .filter(|old| contiguous && old.epoch == remote.epoch)
            .map_or_else(BTreeSet::new, |old| {
                old.runs
                    .keys()
                    .copied()
                    .filter(|key| !remote.runs.contains_key(key))
                    .collect()
            });
        let runs = observations(&remote);
        self.store
            .update(|layout| Ok(layout.reconcile(&runs, &removed)?))
            .map_err(store_fault)?;
        #[cfg(windows)]
        self.terminals.retain(|client, terminal| {
            terminal.epoch == remote.epoch
                && (remote.sequence < terminal.commit_sequence
                    || remote.bindings.get(client) == Some(&terminal.revision))
        });
        self.remote = Some(remote);
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
