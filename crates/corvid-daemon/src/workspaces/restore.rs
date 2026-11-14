use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use corvid_core::agent::{AgentSessionKey, Provider};
use corvid_core::layout::Location;
use corvid_protocol::wire::{
    AgentRestoreMode, PaneRestoreResult, PaneRestoreStatus, RestoreWorkspace, WorkspaceRestored,
};
use tokio::time::{Instant, sleep};
use uuid::Uuid;

use super::{Launch, ProtocolFault, Registry, Reply, Workspaces};
use crate::metadata::SavedSession;
use crate::projection;
use crate::terminal::TerminalHandle;

const CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct Attempt {
    request: String,
    result: PaneRestoreResult,
}

impl Attempt {
    pub(super) fn is_starting(&self) -> bool {
        self.result.status() == PaneRestoreStatus::Starting
    }
}

impl Workspaces {
    /// Reservation and launch decisions are atomic with ordinary pane operations.
    /// The bounded background attempt belongs to the daemon, not the requesting socket.
    pub(crate) fn restore_workspace(
        &self,
        request: &RestoreWorkspace,
        request_id: &str,
    ) -> Reply<WorkspaceRestored> {
        let invalid =
            || ProtocolFault::new("InvalidMessage", "invalid workspace restoration request");
        let workspace = parse_id(&request.workspace_id).ok_or_else(invalid)?;
        let pane = request
            .pane_id
            .as_deref()
            .map(|id| parse_id(id).ok_or_else(invalid))
            .transpose()?;
        let mode = AgentRestoreMode::try_from(request.mode).map_err(|_| invalid())?;
        if mode == AgentRestoreMode::Unspecified {
            return Err(invalid());
        }
        let mut registry = self.lock_registry();
        registry.sweep();
        let layout = self.metadata.layout();
        let locations: Vec<_> = layout
            .locations()
            .filter(|location| {
                location.workspace == workspace && pane.is_none_or(|pane| location.pane == pane)
            })
            .collect();
        if locations.is_empty() {
            return Err(super::target_gone());
        }
        let mut panes = Vec::with_capacity(locations.len());
        for location in locations {
            let mut result = registry.restore_result(location);
            let status = result.status();
            let duplicate = registry
                .restores
                .get(&location.pane)
                .is_some_and(|attempt| attempt.request == request_id);
            let start = !duplicate
                && match mode {
                    AgentRestoreMode::Resume => status == PaneRestoreStatus::Available,
                    AgentRestoreMode::Retry => matches!(
                        status,
                        PaneRestoreStatus::Available | PaneRestoreStatus::Failed
                    ),
                    _ => false,
                };
            if start {
                let prepared = registry
                    .check_not_closing(location)
                    .and_then(|()| prepare_launch(self.metadata.saved_session(location.pane)))
                    .and_then(|(launch, key)| {
                        self.reserve_slot(&mut registry, location, true)
                            .map(|terminal| (launch, key, terminal))
                    });
                match prepared {
                    Ok((launch, key, terminal)) => {
                        result = pane_result(location, PaneRestoreStatus::Starting);
                        registry.restores.insert(
                            location.pane,
                            Attempt {
                                request: request_id.to_owned(),
                                result: result.clone(),
                            },
                        );
                        let workspaces = self.clone();
                        tokio::spawn(async move {
                            workspaces
                                .run_restore(location, terminal, launch, key)
                                .await;
                        });
                    }
                    Err(error) => {
                        result = failed(location, error);
                        registry.restores.insert(
                            location.pane,
                            Attempt {
                                request: request_id.to_owned(),
                                result: result.clone(),
                            },
                        );
                    }
                }
            }
            panes.push(result);
        }
        Ok(WorkspaceRestored { panes })
    }

    async fn run_restore(
        &self,
        location: Location,
        terminal: TerminalHandle,
        launch: Launch,
        key: AgentSessionKey,
    ) {
        let started = terminal.restore(launch).await;
        let (result, release) = match started {
            Ok(id) => {
                let confirmed = self.confirm_restore(id, &terminal, &key).await;
                match confirmed {
                    Ok(()) => {
                        let mut result = pane_result(location, PaneRestoreStatus::Resumed);
                        result.terminal_id = Some(id.to_string());
                        (result, true)
                    }
                    Err(error) => {
                        // Only this attempt's unclaimed terminal may be stopped.
                        let stopped = terminal.cancel_restore(id).await;
                        (
                            failed(location, stopped.as_ref().err().cloned().unwrap_or(error)),
                            stopped.is_ok(),
                        )
                    }
                }
            }
            Err(error) => (failed(location, error), true),
        };
        let mut registry = self.lock_registry();
        if release && let Some(slot) = registry.slots.get_mut(&location.pane) {
            slot.reserved = false;
        }
        if let Some(attempt) = registry.restores.get_mut(&location.pane) {
            attempt.result = result;
        }
    }

    async fn confirm_restore(
        &self,
        id: Uuid,
        terminal: &TerminalHandle,
        key: &AgentSessionKey,
    ) -> Reply<()> {
        let deadline = Instant::now() + CONFIRM_TIMEOUT;
        loop {
            if !terminal.has_live_terminal() {
                return Err(ProtocolFault::new(
                    "ResumeExited",
                    "provider exited before confirming the saved session; check session availability on the daemon host",
                ));
            }
            if let Some(observed) = self.lock_registry().agents.confirmed_session(id) {
                return if observed == *key {
                    Ok(())
                } else {
                    Err(ProtocolFault::new(
                        "ResumeSessionMismatch",
                        "provider opened a different session; the restore attempt was stopped",
                    ))
                };
            }
            if Instant::now() >= deadline {
                return Err(ProtocolFault::new(
                    "ResumeUnconfirmed",
                    "provider did not confirm the session; check login, provider version, and observation hooks",
                ));
            }
            sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Registry {
    fn restore_result(&self, location: Location) -> PaneRestoreResult {
        let attempt = self.restores.get(&location.pane);
        if let Some(attempt) =
            attempt.filter(|attempt| attempt.result.status() == PaneRestoreStatus::Starting)
        {
            return attempt.result.clone();
        }
        if self.has_live_at(location) {
            let mut result = pane_result(location, PaneRestoreStatus::Existing);
            result.terminal_id = self
                .terminals
                .iter()
                .find_map(|(id, current)| (*current == location).then(|| id.to_string()));
            if let Some(attempt) = attempt.filter(|attempt| {
                attempt.result.status() == PaneRestoreStatus::Resumed
                    && attempt.result.terminal_id == result.terminal_id
            }) {
                return attempt.result.clone();
            }
            return result;
        }
        if let Some(attempt) =
            attempt.filter(|attempt| attempt.result.status() == PaneRestoreStatus::Failed)
        {
            return attempt.result.clone();
        }
        match self.metadata.saved_session(location.pane) {
            None => pane_result(location, PaneRestoreStatus::Empty),
            Some(saved) if saved.verified => pane_result(location, PaneRestoreStatus::Available),
            Some(_) => failed(
                location,
                ProtocolFault::new(
                    "SessionUnverified",
                    "the last conversation transition could not be verified; open a shell to choose the session",
                ),
            ),
        }
    }
}

fn prepare_launch(saved: Option<SavedSession>) -> Reply<(Launch, AgentSessionKey)> {
    let saved = saved
        .filter(|saved| saved.verified && saved.key.is_valid())
        .ok_or_else(|| {
            ProtocolFault::new(
                "SessionUnverified",
                "no verified saved conversation is available for this pane",
            )
        })?;
    let cwd = saved.cwd.as_ref()
        .and_then(|cwd| crate::platform::decode_os_string(&cwd.path))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_dir())
        .ok_or_else(|| ProtocolFault::new("ResumeDirectoryUnavailable", "the saved session directory is unavailable on the daemon host; restore it or open a shell"))?;
    let argument = match saved.key.provider {
        Provider::Codex => "resume",
        Provider::Claude => "--resume",
    };
    let launch = Launch {
        argv: vec![
            OsString::from(argument),
            OsString::from(&saved.key.provider_session_id),
        ],
        cwd: Some(cwd),
        provider: Some(saved.key.provider),
        if_empty: true,
    };
    Ok((launch, saved.key))
}

fn parse_id(id: &str) -> Option<Uuid> {
    Uuid::parse_str(id).ok().filter(|id| !id.is_nil())
}

fn pane_result(location: Location, status: PaneRestoreStatus) -> PaneRestoreResult {
    PaneRestoreResult {
        location: Some(projection::selection(location)),
        status: status as i32,
        ..Default::default()
    }
}

fn failed(location: Location, error: ProtocolFault) -> PaneRestoreResult {
    PaneRestoreResult {
        error: Some(error.into_wire()),
        ..pane_result(location, PaneRestoreStatus::Failed)
    }
}
