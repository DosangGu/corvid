mod actions;
mod presentation;

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use corvid_client::{ControlEndpoint, SnapshotWatch};
use corvid_protocol::ProtocolFault;
use corvid_protocol::wire::CompanionConnection;
use corvid_protocol::wire::envelope::Body;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep, sleep_until};
use uuid::Uuid;

use crate::last_known::LastKnown;
#[cfg(windows)]
use crate::platform::{TerminalRecord, TerminalRegistry};
use crate::remote::Remote;
use crate::{RunKey, RunObservation, Store};

type Outcome = Result<Body, ProtocolFault>;
type Observations = BTreeMap<RunKey, RunObservation>;

const STATUS_SAVE_DELAY: Duration = Duration::from_secs(1);

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
            let fault = match self.flush_status() {
                Ok(()) => fault,
                Err(error) => store_fault(error).into(),
            };
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
                _ = sleep_until(self.save_status_at.unwrap_or_else(Instant::now)), if self.save_status_at.is_some() => {
                    if let Err(error) = self.flush_status() {
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
        // Only authoritative layout deletion retires a tracked card. Agent
        // completion, entity pruning, transport loss, and epoch changes do not.
        let removed = self.store.layout().closed_panes(&remote.panes);
        let runs = observations(&remote);
        self.store
            .update(|layout| Ok(layout.reconcile(&runs, &removed)?))
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
