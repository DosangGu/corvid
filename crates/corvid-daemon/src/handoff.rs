pub(crate) mod input;

use std::path::Path;

use anyhow::{Result, ensure};
use corvid_core::layout::Location;
use serde::{Deserialize, Serialize};

use crate::identity::Identity;
use crate::metadata::MetadataView;
use crate::platform::StateDirectory;
use crate::terminal::RetainedTerminal;
use crate::workspaces::{MAX_PTY_WORKERS, Workspaces};

const FILE: &str = "handoff.json";
const LIMIT: u64 = 4 * 1024 * 1024;

#[derive(Deserialize, Serialize)]
struct Handoff {
    version: u32,
    host_id: String,
    epoch: String,
    terminals: Vec<(Location, RetainedTerminal)>,
}

pub(crate) async fn prepare(
    runtime: &Path,
    identity: &Identity,
    workspaces: &Workspaces,
    mut metadata: MetadataView,
    clients: &mut crate::executor::PeerTasks,
    preserve_terminals: bool,
) -> Result<()> {
    while clients.try_join_next().is_some() {}
    ensure!(
        preserve_terminals || (clients.is_empty() && workspaces.is_idle().await),
        corvid_protocol::ProtocolFault::new("DaemonBusy", "this updater requires an idle daemon")
    );
    workspaces.retain_terminals(true).await?;
    clients.shutdown().await;
    let result = async {
        ensure!(
            crate::executor::time::timeout(corvid_protocol::COMMAND_TIMEOUT, metadata.wait_saved())
                .await
                .unwrap_or(false),
            corvid_protocol::ProtocolFault::new(
                "PersistenceUnavailable",
                "daemon metadata could not be saved before handoff"
            )
        );
        let handoff = Handoff {
            version: 1,
            host_id: identity.host_id.clone(),
            epoch: identity.epoch.clone(),
            terminals: workspaces.retain_terminals(true).await?,
        };
        let bytes = serde_json::to_vec(&handoff)?;
        ensure!(
            bytes.len() as u64 <= LIMIT,
            "daemon handoff exceeds its bound"
        );
        StateDirectory::open(runtime)?.replace_private_file(FILE, &bytes)
    }
    .await;
    if result.is_err() {
        let _ = workspaces.retain_terminals(false).await;
    }
    result
}

pub(crate) async fn cancel(
    runtime: &Path,
    identity: &Identity,
    workspaces: &Workspaces,
) -> Result<()> {
    let _ = workspaces.retain_terminals(false).await;
    let handoff = Handoff {
        version: 1,
        host_id: identity.host_id.clone(),
        epoch: identity.epoch.clone(),
        terminals: Vec::new(),
    };
    StateDirectory::open(runtime)?.replace_private_file(FILE, &serde_json::to_vec(&handoff)?)
}

pub(crate) async fn recover(
    runtime: &Path,
    identity: &Identity,
    workspaces: &Workspaces,
) -> Result<()> {
    let directory = StateDirectory::open(runtime)?;
    let Some(bytes) = directory.read_private_file(FILE, LIMIT)? else {
        return Ok(());
    };
    let mut handoff: Handoff = serde_json::from_slice(&bytes)?;
    ensure!(
        handoff.version == 1
            && handoff.host_id == identity.host_id
            && handoff.terminals.len() <= MAX_PTY_WORKERS,
        "invalid daemon handoff identity or version"
    );
    if handoff.terminals.is_empty() {
        return Ok(());
    }
    ensure!(
        handoff.epoch != identity.epoch,
        "cannot adopt the current daemon handoff"
    );
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let mut associations = std::collections::HashSet::new();
        ensure!(
            handoff
                .terminals
                .iter()
                .filter_map(|(_, terminal)| terminal.agent.as_ref())
                .all(|agent| agent.valid() && associations.insert(agent.association)),
            "invalid retained agent identity"
        );
    }
    let mut panes = std::collections::HashSet::new();
    let mut ids = std::collections::HashSet::new();
    ensure!(
        handoff
            .terminals
            .iter()
            .all(|(location, terminal)| { panes.insert(location.pane) && ids.insert(terminal.id) }),
        "duplicate terminal handoff identity"
    );
    #[cfg(windows)]
    {
        let epoch = uuid::Uuid::parse_str(&handoff.epoch)?;
        ensure!(!epoch.is_nil(), "invalid daemon handoff epoch");
        for (_, terminal) in &mut handoff.terminals {
            let observer_epoch = terminal.observer_epoch.get_or_insert(epoch);
            ensure!(
                !observer_epoch.is_nil()
                    && terminal
                        .observer_root
                        .is_none_or(crate::native_process::ProcessKey::valid)
                    && (terminal.observer_root.is_some() || terminal.observer_contexts.is_empty())
                    && crate::agents::RetainedContext::valid_all(&terminal.observer_contexts),
                "invalid retained Windows observer identity"
            );
        }
    }
    let terminals = std::mem::take(&mut handoff.terminals);
    let recovered = async {
        workspaces.recover_terminals(terminals).await?;
        directory.replace_private_file(FILE, &serde_json::to_vec(&handoff)?)
    }
    .await;
    if recovered.is_err() {
        let _ = workspaces.retain_terminals(true).await;
    }
    recovered
}
