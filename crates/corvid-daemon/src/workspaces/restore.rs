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
use crate::provider::{self, Resume, ResumeConfirmation};
use crate::terminal::TerminalHandle;

const CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

struct RestorePlan {
    launch: Launch,
    session: AgentSessionKey,
    confirmation: ResumeConfirmation,
}

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
    pub(crate) async fn restore_workspace(
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
        let request_id = request_id.to_owned();
        self.transaction(move |registry, workspaces| {
            registry.sweep();
            let layout = registry.metadata.layout();
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
                        .and_then(|()| {
                            prepare_launch(registry.metadata.saved_session(location.pane))
                        })
                        .and_then(|plan| {
                            workspaces
                                .reserve_slot(registry, location, true)
                                .map(|terminal| (plan, terminal))
                        });
                    match prepared {
                        Ok((plan, terminal)) => {
                            result = pane_result(location, PaneRestoreStatus::Starting);
                            registry.restores.insert(
                                location.pane,
                                Attempt {
                                    request: request_id.to_owned(),
                                    result: result.clone(),
                                },
                            );
                            let workspaces = workspaces.clone();
                            tokio::spawn(async move {
                                workspaces.run_restore(location, terminal, plan).await;
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
        })
        .await
    }

    async fn run_restore(&self, location: Location, terminal: TerminalHandle, plan: RestorePlan) {
        let launch = plan.launch;
        let directory_exists = {
            let cwd = launch.cwd.clone();
            tokio::task::spawn_blocking(move || cwd.is_some_and(|cwd| cwd.is_dir()))
                .await
                .unwrap_or(false)
        };
        let started = if directory_exists {
            terminal.restore(launch).await
        } else {
            Err(resume_directory_unavailable())
        };
        let (result, release) = match started {
            Ok(id) => {
                let confirmed = match plan.confirmation {
                    ResumeConfirmation::AuthenticatedSession => {
                        self.confirm_restore(id, &terminal, &plan.session).await
                    }
                };
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
        self.registry
            .request(move |registry| {
                if release && let Some(slot) = registry.slots.get_mut(&location.pane) {
                    slot.reserved = false;
                }
                if let Some(attempt) = registry.restores.get_mut(&location.pane) {
                    attempt.result = result;
                }
            })
            .await;
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
            if let Some(observed) = self
                .registry
                .request(move |registry| registry.agents.confirmed_session(id))
                .await
            {
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
            Some(saved) if saved.verified => match resume_contract(saved.key.provider) {
                Ok(_) => pane_result(location, PaneRestoreStatus::Available),
                Err(error) => failed(location, error),
            },
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

fn prepare_launch(saved: Option<SavedSession>) -> Reply<RestorePlan> {
    let saved = saved
        .filter(|saved| saved.verified && provider::valid_session(&saved.key))
        .ok_or_else(|| {
            ProtocolFault::new(
                "SessionUnverified",
                "no verified saved conversation is available for this pane",
            )
        })?;
    let (resume, confirmation) = resume_contract(saved.key.provider)?;
    let cwd = saved
        .cwd
        .as_ref()
        .and_then(|cwd| crate::platform::decode_os_string(&cwd.path))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(resume_directory_unavailable)?;
    let launch = Launch {
        argv: (resume.arguments)(&saved.key.provider_session_id),
        cwd: Some(cwd),
        provider: Some(saved.key.provider),
        if_empty: true,
    };
    Ok(RestorePlan {
        launch,
        session: saved.key,
        confirmation,
    })
}

fn resume_directory_unavailable() -> ProtocolFault {
    ProtocolFault::new(
        "ResumeDirectoryUnavailable",
        "the saved session directory is unavailable on the daemon host; restore it or open a shell",
    )
}

fn resume_contract(provider: Provider) -> Reply<(&'static Resume, ResumeConfirmation)> {
    let resume = provider::contract(provider)
        .resume
        .as_ref()
        .ok_or_else(|| {
            ProtocolFault::new(
                "ResumeUnsupported",
                "provider does not support native resume; open a shell to continue",
            )
        })?;
    let confirmation = resume.confirmation.ok_or_else(|| {
        ProtocolFault::new(
            "ResumeUnconfirmed",
            "provider cannot verify the saved session; open a shell to choose the session",
        )
    })?;
    Ok((resume, confirmation))
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

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::sync::Arc;

    use corvid_core::layout::{CwdObservation, CwdSource};
    use tokio::time::timeout;

    use super::*;
    use crate::collector::HookEnvironment;
    use crate::identity::Identity;
    use crate::metadata;
    use crate::platform::StateDirectory;

    #[tokio::test]
    async fn registered_fixture_recovers_sessions_and_reports_per_pane_limits() -> anyhow::Result<()>
    {
        let directory = tempfile::tempdir_in(
            std::env::home_dir().ok_or_else(|| anyhow::anyhow!("home directory unavailable"))?,
        )?;
        let open = || StateDirectory::create_all(&directory.path().join("state"));
        let state = open()?;
        let lock = state.lock_storage()?;
        let (mut view, updates, saver) = metadata::start(state, lock)?;
        let mut layout = (*updates.layout()).clone();
        let exact = layout.primary().unwrap();
        let manual = layout.add_tab(exact.workspace, "Manual".into())?;
        let unverifiable = layout.add_tab(exact.workspace, "Unverifiable".into())?;
        let empty = layout.add_tab(exact.workspace, "Shell".into())?;
        for location in [exact, manual, unverifiable] {
            layout.record_cwd(
                location,
                CwdObservation {
                    path: directory.path().as_os_str().as_encoded_bytes().to_vec(),
                    source: CwdSource::LaunchDirectory,
                    observed_unix_ms: 1,
                },
            )?;
        }
        updates.update(Some(Arc::new(layout)), std::iter::empty());
        for (location, provider) in [
            (exact, Provider::Fixture),
            (manual, Provider::FixtureManual),
            (unverifiable, Provider::FixtureUnverified),
        ] {
            updates.observe_session(
                location,
                Some(AgentSessionKey {
                    provider,
                    provider_session_id: "session/saved".into(),
                }),
            );
        }
        assert!(timeout(Duration::from_secs(5), view.wait_saved()).await?);
        drop(updates);
        drop(view);
        timeout(Duration::from_secs(5), saver).await??;

        let state = open()?;
        let identity = Identity::load(&state)?;
        let lock = state.lock_storage()?;
        let (_view, updates, _saver) = metadata::start(state, lock)?;
        let plan = prepare_launch(updates.saved_session(exact.pane)).unwrap();
        assert_eq!(plan.session.provider, Provider::Fixture);
        assert_eq!(
            plan.launch.argv,
            [OsStr::new("reopen"), OsStr::new("session/saved")]
        );
        assert_eq!(plan.launch.cwd.as_deref(), Some(directory.path()));

        let workspaces = Workspaces::new(
            updates,
            Arc::new(HookEnvironment {
                runtime: directory.path().join("run"),
                identity: Arc::new(identity),
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                shell_commands: None,
            }),
        );
        let inspect = RestoreWorkspace {
            workspace_id: exact.workspace.to_string(),
            pane_id: None,
            mode: AgentRestoreMode::Inspect as i32,
        };
        let result = workspaces
            .restore_workspace(&inspect, "inspect")
            .await
            .unwrap();
        for (location, status, error_code) in [
            (exact, PaneRestoreStatus::Available, None),
            (manual, PaneRestoreStatus::Failed, Some("ResumeUnsupported")),
            (
                unverifiable,
                PaneRestoreStatus::Failed,
                Some("ResumeUnconfirmed"),
            ),
            (empty, PaneRestoreStatus::Empty, None),
        ] {
            let selection = projection::selection(location);
            let pane = result
                .panes
                .iter()
                .find(|pane| pane.location.as_ref() == Some(&selection))
                .unwrap();
            assert_eq!(pane.status(), status);
            assert_eq!(
                pane.error.as_ref().map(|error| error.code.as_str()),
                error_code
            );
        }
        let retry = RestoreWorkspace {
            pane_id: Some(unverifiable.pane.to_string()),
            mode: AgentRestoreMode::Retry as i32,
            ..inspect
        };
        for _ in 0..2 {
            let result = workspaces.restore_workspace(&retry, "retry").await.unwrap();
            assert_eq!(result.panes[0].status(), PaneRestoreStatus::Failed);
            assert_eq!(
                result.panes[0].error.as_ref().unwrap().code,
                "ResumeUnconfirmed"
            );
        }
        assert!(
            workspaces
                .registry
                .request(|registry| registry.slots.is_empty())
                .await
        );
        Ok(())
    }
}
