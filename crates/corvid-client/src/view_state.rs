//! Private, best-effort client preferences. These are navigation hints, never authority.

use std::env;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_core::layout::Location;
use corvid_protocol::ProtocolFault;
use corvid_protocol::snapshot::SynchronizedSnapshot;
use corvid_protocol::wire::ViewSelection;
use corvid_protocol::wire::layout_node_state::Body as Node;
use corvid_protocol::wire::snapshot_entity::Body as Entity;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use uuid::Uuid;

use crate::{SshAlias, WslDistribution};

#[cfg(target_os = "linux")]
use corvid_protocol::unix::RuntimeDir as Directory;
#[cfg(windows)]
use corvid_protocol::windows::StateDirectory as Directory;

const FILE: &str = "view.json";
const BACKUP: &str = "view.json.bak";
const MAX_BYTES: u64 = 64 * 1024;
const MAX_TARGETS: usize = 64;
const SAVE_DELAY: Duration = Duration::from_millis(250);

#[cfg(test)]
mod tests;

pub mod windows;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    Local,
    Ssh { alias: String },
    Wsl { distribution: String },
    Tui { runtime: PathBuf },
}

impl Target {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Local => {}
            Self::Ssh { alias } => {
                SshAlias::new(alias.clone())?;
            }
            Self::Wsl { distribution } => {
                WslDistribution::new(distribution.clone())?;
            }
            Self::Tui { runtime } => ensure!(
                runtime.is_absolute() && runtime.as_os_str().len() <= 4096,
                "invalid saved runtime directory"
            ),
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedView {
    pub target: Target,
    pub host_id: Uuid,
    pub location: Location,
}

impl SavedView {
    pub fn new(target: Target, host: &str, selection: &ViewSelection) -> Result<Self> {
        target.validate()?;
        Ok(Self {
            target,
            host_id: Uuid::parse_str(host)?,
            location: Location {
                workspace: Uuid::parse_str(&selection.workspace_id)?,
                tab: Uuid::parse_str(&selection.tab_id)?,
                pane: Uuid::parse_str(&selection.pane_id)?,
            },
        })
    }

    /// Resolve only against a fresh snapshot from the verified host and epoch.
    /// A surviving but empty pane stays empty; restoration must not start a process.
    pub fn resolve(&self, snapshot: &SynchronizedSnapshot) -> Result<ViewSelection> {
        ensure!(
            snapshot.session().host_id == self.host_id.to_string(),
            ProtocolFault::new("HostMismatch", "the saved view belongs to a different host")
        );
        let workspace = snapshot
            .snapshot()
            .entities()
            .find_map(|entity| match &entity.body {
                Some(Entity::Workspace(workspace))
                    if workspace.workspace_id == self.location.workspace.to_string() =>
                {
                    Some(workspace)
                }
                _ => None,
            })
            .ok_or_else(|| {
                ProtocolFault::new(
                    "SavedWorkspaceGone",
                    "The saved workspace was removed. Choose a workspace to continue.",
                )
            })?;
        let tab = workspace
            .tabs
            .iter()
            .find(|tab| tab.tab_id == self.location.tab.to_string())
            .or_else(|| workspace.tabs.first())
            .ok_or_else(|| {
                ProtocolFault::new("SavedWorkspaceGone", "The saved workspace has no tabs.")
            })?;
        let panes: Vec<_> = tab
            .nodes
            .iter()
            .filter_map(|node| match &node.body {
                Some(Node::PaneId(pane)) => Some(pane),
                _ => None,
            })
            .collect();
        let saved_pane = self.location.pane.to_string();
        let pane = panes
            .iter()
            .find(|pane| pane.as_str() == saved_pane)
            .or_else(|| panes.first())
            .ok_or_else(|| {
                ProtocolFault::new("SavedWorkspaceGone", "The saved tab has no panes.")
            })?;
        Ok(ViewSelection {
            workspace_id: workspace.workspace_id.clone(),
            tab_id: tab.tab_id.clone(),
            pane_id: (*pane).clone(),
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Stored {
    schema_version: u32,
    /// Most recently selected target first; at most one view per connection target.
    views: Vec<SavedView>,
}

impl Default for Stored {
    fn default() -> Self {
        Self {
            schema_version: 1,
            views: Vec::new(),
        }
    }
}

fn decode(bytes: &[u8]) -> Result<Stored> {
    let stored: Stored = serde_json::from_slice(bytes)?;
    ensure!(stored.schema_version == 1, "unsupported client view schema");
    ensure!(
        stored.views.len() <= MAX_TARGETS,
        "too many saved connection targets"
    );
    for view in &stored.views {
        view.target.validate()?;
    }
    Ok(stored)
}

fn require_supported_schema(bytes: &[u8]) -> Result<()> {
    let value = serde_json::from_slice::<serde_json::Value>(bytes).ok();
    ensure!(
        value
            .as_ref()
            .and_then(|value| value["schema_version"].as_u64())
            .is_none_or(|version| version == 1),
        "unsupported client view schema"
    );
    Ok(())
}

fn retry_storage<T>(operation: impl Fn() -> Result<T>) -> Result<T> {
    let mut attempts = 0;
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if attempts == 20 => return Err(error),
            Err(_) => {
                attempts += 1;
                thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

/// A separate directory per presentation keeps Desktop and TUI records independent.
#[derive(Clone)]
pub struct ViewStore {
    path: PathBuf,
}

impl ViewStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn last(&self) -> Option<SavedView> {
        self.read()
            .and_then(|stored| stored.views.into_iter().next())
    }

    pub fn for_target(&self, target: &Target) -> Option<SavedView> {
        self.read()
            .and_then(|stored| stored.views.into_iter().find(|view| &view.target == target))
    }

    fn read(&self) -> Option<Stored> {
        if !self.path.exists() {
            return None;
        }
        let result = (|| {
            let directory = Directory::open(&self.path)?;
            let Some(bytes) = directory.read_private_file(FILE, MAX_BYTES)? else {
                return Ok(Stored::default());
            };
            match decode(&bytes) {
                Ok(stored) => Ok(stored),
                Err(_) => {
                    // Never reinterpret or overwrite a future schema as an older one.
                    require_supported_schema(&bytes)?;
                    directory
                        .read_private_file(BACKUP, MAX_BYTES)?
                        .map(|bytes| decode(&bytes))
                        .transpose()?
                        .context("invalid saved client view")
                }
            }
        })();
        match result {
            Ok(stored) => Some(stored),
            Err(error) => {
                eprintln!("corvid: could not read saved view: {error:#}");
                None
            }
        }
    }

    fn save(&self, view: &SavedView) -> Result<()> {
        // Another first-time client may create the directory between open and mkdir.
        let directory = retry_storage(|| Directory::create_all(&self.path))?;
        // The lock is held only during a read/merge/replace, not for a client lifetime.
        // Multiple clients may update different hosts without dropping each other's records.
        let _lock = retry_storage(|| directory.lock_storage())?;
        let previous = directory.read_private_file(FILE, MAX_BYTES)?;
        let mut stored = match previous.as_deref() {
            None => Stored::default(),
            Some(bytes) => match decode(bytes) {
                Ok(stored) => {
                    directory.replace_private_file(BACKUP, bytes)?;
                    stored
                }
                Err(_) => {
                    require_supported_schema(bytes)?;
                    directory.replace_private_file("view.json.corrupt", bytes)?;
                    directory
                        .read_private_file(BACKUP, MAX_BYTES)?
                        .and_then(|bytes| decode(&bytes).ok())
                        .unwrap_or_default()
                }
            },
        };
        stored.views.retain(|old| old.target != view.target);
        stored.views.insert(0, view.clone());
        stored.views.truncate(MAX_TARGETS);
        let bytes = serde_json::to_vec(&stored)?;
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "saved views exceed the size limit"
        );
        directory.replace_private_file(FILE, &bytes)
    }

    pub fn recorder(&self) -> Result<Recorder> {
        let (updates, mut incoming) = watch::channel::<Option<SavedView>>(None);
        let store = self.clone();
        let worker = thread::Builder::new()
            .name("corvid-view-save".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()?;
                runtime.block_on(async {
                    while incoming.changed().await.is_ok() {
                        tokio::time::sleep(SAVE_DELAY).await;
                        let view = incoming.borrow_and_update().clone();
                        if let Some(view) = view {
                            store.save(&view)?;
                        }
                    }
                    Ok(())
                })
            })?;
        Ok(Recorder {
            updates: Some(updates),
            worker: Some(worker),
        })
    }
}

pub struct Recorder {
    updates: Option<watch::Sender<Option<SavedView>>>,
    worker: Option<thread::JoinHandle<Result<()>>>,
}

impl Recorder {
    pub fn record(&self, target: Target, host: &str, selection: &ViewSelection) {
        if let Ok(view) = SavedView::new(target, host, selection)
            && let Some(updates) = &self.updates
        {
            updates.send_if_modified(|previous| {
                if previous.as_ref() == Some(&view) {
                    return false;
                }
                *previous = Some(view);
                true
            });
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.updates.take();
        if let Some(worker) = self.worker.take()
            && let Ok(Err(error)) = worker.join()
        {
            eprintln!("corvid: could not save the last view: {error:#}");
        }
    }
}

pub fn tui_store() -> Option<ViewStore> {
    let base = env::var_os("CORVID_VIEW_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            #[cfg(windows)]
            {
                env::var_os("LOCALAPPDATA")
                    .filter(|base| !base.is_empty())
                    .map(|base| PathBuf::from(base).join("Corvid/clients"))
            }
            #[cfg(target_os = "linux")]
            {
                env::var_os("XDG_STATE_HOME")
                    .filter(|base| !base.is_empty())
                    .map(PathBuf::from)
                    .or_else(|| {
                        env::var_os("HOME")
                            .filter(|home| !home.is_empty())
                            .map(|home| PathBuf::from(home).join(".local/state"))
                    })
                    .map(|base| base.join("corvid/clients"))
            }
        })?;
    Some(ViewStore::new(base.join("tui")))
}

pub fn tui_runtime(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if override_path.is_some() || env::var_os("CORVID_RUNTIME_DIR").is_some() {
        return crate::runtime_path(override_path);
    }
    if let Some(view) = tui_store().and_then(|store| store.last())
        && let Target::Tui { runtime } = view.target
    {
        return Ok(runtime);
    }
    crate::runtime_path(None)
}

pub fn tui_target(runtime: &Path) -> Result<Target> {
    Ok(Target::Tui {
        runtime: std::path::absolute(runtime)?,
    })
}
