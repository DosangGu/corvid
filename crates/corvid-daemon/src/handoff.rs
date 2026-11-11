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
const LIMIT: u64 = 64 * 1024;

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
    clients: &mut tokio::task::JoinSet<anyhow::Result<()>>,
    preserve_terminals: bool,
) -> Result<()> {
    while clients.try_join_next().is_some() {}
    ensure!(
        preserve_terminals || (clients.is_empty() && workspaces.is_idle()),
        corvid_protocol::ProtocolFault::new("DaemonBusy", "this updater requires an idle daemon")
    );
    workspaces.retain_terminals(true).await?;
    clients.shutdown().await;
    let result = async {
        ensure!(
            tokio::time::timeout(corvid_protocol::COMMAND_TIMEOUT, metadata.wait_saved())
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
        StateDirectory::open(runtime)?.replace_private_file(FILE, &serde_json::to_vec(&handoff)?)
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
    let mut panes = std::collections::HashSet::new();
    let mut ids = std::collections::HashSet::new();
    ensure!(
        handoff
            .terminals
            .iter()
            .all(|(location, terminal)| { panes.insert(location.pane) && ids.insert(terminal.id) }),
        "duplicate terminal handoff identity"
    );
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
