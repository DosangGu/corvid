use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use corvid_core::agent::ProcessLiveness;
use corvid_protocol::ProtocolFault;
use corvid_protocol::wire::{AgentProvider, RegisterShellAgent};
use uuid::Uuid;

use crate::process::{self, Executable, Incarnation, ProcessImage};

use super::{Provider, Reply, Workspaces};

const EXEC_GRACE: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(super) struct Shells {
    terminals: BTreeMap<Uuid, Shell>,
    sampled: Uuid,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Shell {
    root: Incarnation,
    agent: Option<ShellAgent>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ShellAgent {
    association: Uuid,
    owner: ProcessImage,
    registered: Instant,
}

impl ShellAgent {
    fn stopped(self) -> Option<ProcessLiveness> {
        self.owner
            .shell_exit(self.registered.elapsed() < EXEC_GRACE)
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

    fn next_sample(&mut self) -> Option<(Uuid, ShellAgent)> {
        let (&terminal, agent) = self
            .terminals
            .iter()
            .filter_map(|(terminal, shell)| Some((terminal, shell.agent?)))
            .min_by_key(|(terminal, _)| (**terminal <= self.sampled, **terminal))?;
        self.sampled = terminal;
        Some((terminal, agent))
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
        let provider = match AgentProvider::try_from(request.provider) {
            Ok(AgentProvider::Claude) => Provider::Claude,
            Ok(AgentProvider::Codex) => Provider::Codex,
            _ => return Err(unauthorized()),
        };
        if request.executable_path.len() > 4096 || request.executable_path.contains(&0) {
            return Err(unauthorized());
        }
        let path = PathBuf::from(OsString::from_vec(request.executable_path.clone()));
        if !path.is_absolute() {
            return Err(unauthorized());
        }
        let shell = *self
            .lock_registry()
            .shells
            .terminals
            .get(&terminal)
            .ok_or_else(unauthorized)?;

        let (owner, previous) = tokio::task::spawn_blocking(move || {
            let incarnation = process::verify_shell_launcher(shell.root, peer)?;
            let executable = Executable::read(&path)?;
            let previous = match shell.agent {
                Some(agent) => Some(agent.stopped()?),
                None => None,
            };
            Some((
                ProcessImage {
                    incarnation,
                    executable,
                },
                previous,
            ))
        })
        .await
        .ok()
        .flatten()
        .ok_or_else(unauthorized)?;

        let mut registry = self.lock_registry();
        let current = registry
            .shells
            .terminals
            .get(&terminal)
            .ok_or_else(unauthorized)?;
        if current.root != shell.root || current.agent.is_some() && current.agent != shell.agent {
            return Err(unauthorized());
        }
        let location = *registry.terminals.get(&terminal).ok_or_else(unauthorized)?;
        if let Some((agent, liveness)) = current.agent.zip(previous) {
            registry.agents.retire_shell(agent.association, liveness);
            registry.publish(Vec::new());
        }
        let association = registry
            .agents
            .reserve_shell(location, terminal, provider, owner);
        registry.shells.terminals.insert(
            terminal,
            Shell {
                agent: Some(ShellAgent {
                    association,
                    owner,
                    registered: Instant::now(),
                }),
                ..shell
            },
        );
        Ok(association)
    }

    pub(super) fn poll_shell_agents(&self) {
        let sample = self.lock_registry().shells.next_sample();
        let Some((terminal, agent)) = sample else {
            return;
        };
        let Some(liveness) = agent.stopped() else {
            return;
        };
        let mut registry = self.lock_registry();
        if let Some(shell) = registry.shells.terminals.get_mut(&terminal)
            && shell.agent == Some(agent)
        {
            shell.agent = None;
            registry.agents.retire_shell(agent.association, liveness);
        }
    }
}
