use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use corvid_core::agent::{ProcessLiveness, Provider};
use corvid_protocol::ProtocolFault;
use corvid_protocol::wire::RegisterShellAgent;
use uuid::Uuid;

use crate::process::{self, Executable, Incarnation, ProcessImage};
use crate::provider;

use super::{Registry, Reply, Workspaces};

const EXEC_GRACE: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(super) struct Shells {
    terminals: BTreeMap<Uuid, Shell>,
    sampled: Uuid,
}

#[derive(Clone, PartialEq, Eq)]
struct Shell {
    root: Incarnation,
    agent: Option<ShellAgent>,
}

#[derive(Clone, PartialEq, Eq)]
struct ShellAgent {
    association: Uuid,
    owner: ProcessImage,
    restart_path: Option<Arc<Path>>,
    registered: Instant,
}

impl ShellAgent {
    fn stopped(&self) -> Option<ProcessLiveness> {
        self.owner.shell_exit(
            self.registered.elapsed() < EXEC_GRACE,
            self.restart_path.as_deref(),
        )
    }
}

impl Shells {
    pub(super) fn add(&mut self, terminal: Uuid, root: Incarnation) {
        self.terminals.insert(terminal, Shell { root, agent: None });
    }

    pub(super) fn remove(&mut self, terminal: Uuid) -> Option<Uuid> {
        self.terminals
            .remove(&terminal)?
            .agent
            .map(|agent| agent.association)
    }

    pub(super) fn restarted(&mut self, terminal: Uuid, association: Uuid, owner: ProcessImage) {
        if let Some(agent) = self
            .terminals
            .get_mut(&terminal)
            .and_then(|shell| shell.agent.as_mut())
            && agent.association == association
        {
            agent.owner = owner;
        }
    }

    fn next_sample(&mut self) -> Option<(Uuid, ShellAgent)> {
        let (&terminal, agent) = self
            .terminals
            .iter()
            .filter_map(|(terminal, shell)| Some((terminal, shell.agent.as_ref()?)))
            .min_by_key(|(terminal, _)| (**terminal <= self.sampled, **terminal))?;
        self.sampled = terminal;
        Some((terminal, agent.clone()))
    }
}

impl Registry {
    pub(crate) fn recover_agent(
        &mut self,
        terminal: Uuid,
        root: Incarnation,
        location: corvid_core::layout::Location,
        agent: crate::agents::RetainedAgent,
    ) {
        let owner = agent.owner;
        let association = agent.association;
        let restart_path = agent.restart_path();
        if !self.agents.recover(terminal, location, agent) {
            return;
        }
        self.shells.terminals.insert(
            terminal,
            Shell {
                root,
                agent: Some(ShellAgent {
                    association,
                    owner,
                    restart_path,
                    registered: Instant::now() - EXEC_GRACE,
                }),
            },
        );
        self.publish(Vec::new());
    }
}

impl Workspaces {
    pub(crate) async fn register_shell_agent(
        &self,
        request: &RegisterShellAgent,
        peer: u32,
    ) -> Reply<Uuid> {
        let unauthorized =
            || ProtocolFault::new("Unauthorized", "shell launch has no verified PTY");
        let terminal = Uuid::parse_str(&request.terminal_id).map_err(|_| unauthorized())?;
        let provider = provider::from_wire(request.provider).ok_or_else(unauthorized)?;
        if request.executable_path.len() > 4096 || request.executable_path.contains(&0) {
            return Err(unauthorized());
        }
        let path = PathBuf::from(OsString::from_vec(request.executable_path.clone()));
        if !path.is_absolute() {
            return Err(unauthorized());
        }
        let shell = self
            .registry
            .request(move |registry| registry.shells.terminals.get(&terminal).cloned())
            .await
            .ok_or_else(unauthorized)?;

        let sampled_shell = shell.clone();
        let (owner, previous, restart_path) = tokio::task::spawn_blocking(move || {
            let incarnation = process::verify_shell_launcher(sampled_shell.root, peer)?;
            let executable = Executable::read(&path)?;
            let previous = match sampled_shell.agent {
                Some(agent) => Some(agent.stopped()?),
                None => None,
            };
            let restart_path = (provider == Provider::Claude).then(|| Arc::from(path));
            Some((
                ProcessImage {
                    incarnation,
                    executable,
                },
                previous,
                restart_path,
            ))
        })
        .await
        .ok()
        .flatten()
        .ok_or_else(unauthorized)?;

        self.registry
            .request(move |registry| {
                let current = registry
                    .shells
                    .terminals
                    .get(&terminal)
                    .ok_or_else(unauthorized)?;
                if current.root != shell.root
                    || current.agent.is_some() && current.agent != shell.agent
                {
                    return Err(unauthorized());
                }
                let location = *registry.terminals.get(&terminal).ok_or_else(unauthorized)?;
                if let Some((agent, liveness)) = current.agent.as_ref().zip(previous) {
                    registry.agents.retire_shell(agent.association, liveness);
                    registry.publish(Vec::new());
                }
                let association = registry.agents.reserve_shell(
                    location,
                    terminal,
                    provider,
                    owner,
                    restart_path.clone(),
                );
                registry.shells.terminals.insert(
                    terminal,
                    Shell {
                        agent: Some(ShellAgent {
                            association,
                            owner,
                            restart_path,
                            registered: Instant::now(),
                        }),
                        ..shell
                    },
                );
                Ok(association)
            })
            .await
    }

    pub(super) async fn poll_shell_agents(&self) {
        let sample = self
            .registry
            .request(|registry| registry.shells.next_sample())
            .await;
        let Some((terminal, agent)) = sample else {
            return;
        };
        let sampled_agent = agent.clone();
        let Some(liveness) = tokio::task::spawn_blocking(move || sampled_agent.stopped())
            .await
            .ok()
            .flatten()
        else {
            return;
        };
        self.registry
            .request(move |registry| {
                if let Some(shell) = registry.shells.terminals.get_mut(&terminal)
                    && shell.agent.as_ref() == Some(&agent)
                {
                    shell.agent = None;
                    registry.agents.retire_shell(agent.association, liveness);
                }
            })
            .await;
    }
}
