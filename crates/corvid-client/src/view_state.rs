//! Private, best-effort client preferences. These are navigation hints, never authority.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_async::watch;
use corvid_core::layout::Location;
use corvid_protocol::ProtocolFault;
use corvid_protocol::snapshot::SynchronizedSnapshot;
use corvid_protocol::wire::layout_node_state::Body as Node;
use corvid_protocol::wire::snapshot_entity::Body as Entity;
use corvid_protocol::wire::{ViewSelection, WorkspaceState};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{SshAlias, WslDistribution};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use corvid_protocol::unix::RuntimeDir as Directory;
#[cfg(windows)]
use corvid_protocol::windows::StateDirectory as Directory;

const FILE: &str = "view.json";
const BACKUP: &str = "view.json.bak";
const MAX_BYTES: u64 = 64 * 1024;
const MAX_TARGETS: usize = 64;
const MAX_WORKSPACE_VIEWS: usize = 128;
const SAVE_DELAY: Duration = Duration::from_millis(250);
// A writer can hold this lock across multiple durable file replacements.
#[cfg(windows)]
const WINDOWS_SAVE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

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
        let workspace = self.workspace(snapshot).ok_or_else(|| {
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

    /// Prefer the remembered pane, then another eligible pane in its tab.
    /// The caller supplies the normal workspace selection if the tab has no candidate.
    pub fn preferred_pane(
        &self,
        workspace: &WorkspaceState,
        eligible: impl Fn(&ViewSelection) -> bool,
    ) -> Option<ViewSelection> {
        if workspace.workspace_id != self.location.workspace.to_string() {
            return None;
        }
        let saved_tab = self.location.tab.to_string();
        let tab = workspace.tabs.iter().find(|tab| tab.tab_id == saved_tab)?;
        let saved_pane = self.location.pane.to_string();
        let mut first = None;
        for node in &tab.nodes {
            let Some(Node::PaneId(pane_id)) = &node.body else {
                continue;
            };
            let view = ViewSelection {
                workspace_id: workspace.workspace_id.clone(),
                tab_id: tab.tab_id.clone(),
                pane_id: pane_id.clone(),
            };
            if eligible(&view) {
                if *pane_id == saved_pane {
                    return Some(view);
                }
                first.get_or_insert(view);
            }
        }
        first
    }

    pub fn preferred_live_pane(&self, snapshot: &SynchronizedSnapshot) -> Option<ViewSelection> {
        if snapshot.session().host_id != self.host_id.to_string() {
            return None;
        }
        let workspace = self.workspace(snapshot)?;
        let live: Vec<_> = snapshot
            .snapshot()
            .entities()
            .filter_map(|entity| match &entity.body {
                Some(Entity::Terminal(terminal)) => terminal.location.as_ref(),
                _ => None,
            })
            .collect();
        self.preferred_pane(workspace, |view| live.contains(&view))
    }

    fn workspace<'a>(&self, snapshot: &'a SynchronizedSnapshot) -> Option<&'a WorkspaceState> {
        let workspace_id = self.location.workspace.to_string();
        snapshot
            .snapshot()
            .entities()
            .find_map(|entity| match &entity.body {
                Some(Entity::Workspace(workspace)) if workspace.workspace_id == workspace_id => {
                    Some(workspace)
                }
                _ => None,
            })
    }

    fn same_workspace(&self, other: &Self) -> bool {
        self.target == other.target
            && self.host_id == other.host_id
            && self.location.workspace == other.location.workspace
    }
}

/// A remembered target can become unavailable between the snapshot and attach.
pub fn workspace_preference_missed(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ProtocolFault>().is_some_and(|fault| {
        matches!(
            fault.code.as_str(),
            "TargetGone" | "NoTerminal" | "WorkspaceBusy"
        )
    })
}

#[derive(Serialize, Deserialize)]
struct Stored {
    schema_version: u32,
    /// Most recently selected target first; at most one view per connection target.
    views: Vec<SavedView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    workspace_views: Vec<SavedView>,
}

impl Default for Stored {
    fn default() -> Self {
        Self {
            schema_version: 1,
            views: Vec::new(),
            workspace_views: Vec::new(),
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
    ensure!(
        stored.workspace_views.len() <= MAX_WORKSPACE_VIEWS,
        "too many saved workspace views"
    );
    for view in stored.views.iter().chain(&stored.workspace_views) {
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
    shared_workspace_path: Option<PathBuf>,
}

impl ViewStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            shared_workspace_path: None,
        }
    }

    pub fn last(&self) -> Option<SavedView> {
        self.read()
            .and_then(|stored| stored.views.into_iter().next())
    }

    pub fn for_target(&self, target: &Target) -> Option<SavedView> {
        self.read()
            .and_then(|stored| stored.views.into_iter().find(|view| &view.target == target))
    }

    pub fn for_workspace(&self, target: &Target, host: &str, workspace: &str) -> Option<SavedView> {
        let host = Uuid::parse_str(host).ok()?;
        let workspace = Uuid::parse_str(workspace).ok()?;
        let find = |stored: Stored| {
            stored
                .workspace_views
                .into_iter()
                .chain(stored.views)
                .find(|view| {
                    &view.target == target
                        && view.host_id == host
                        && view.location.workspace == workspace
                })
        };
        self.shared_workspace_path
            .as_ref()
            .and_then(|path| Self::new(path.clone()).read())
            .and_then(find)
            .or_else(|| self.read().and_then(find))
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
        self.save_views(std::slice::from_ref(view))
    }

    fn save_views(&self, views: &[SavedView]) -> Result<()> {
        let saved = self.save_batch(views);
        let Some(path) = &self.shared_workspace_path else {
            return saved;
        };
        let shared = Self::new(path.clone()).save_batch(views);
        saved.and(shared)
    }

    fn save_batch(&self, views: &[SavedView]) -> Result<()> {
        // Another first-time client may create the directory between open and mkdir.
        let directory = retry_storage(|| Directory::create_all(&self.path))?;
        // The lock is held only during a read/merge/replace, not for a client lifetime.
        // Multiple clients may update different hosts without dropping each other's records.
        #[cfg(windows)]
        let _lock = directory.lock_storage_with_timeout(WINDOWS_SAVE_LOCK_TIMEOUT)?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let _lock = retry_storage(|| directory.lock_storage())?;
        Self::save_locked(&directory, views)
    }

    fn save_locked(directory: &Directory, views: &[SavedView]) -> Result<()> {
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
        if stored.workspace_views.is_empty() {
            stored.workspace_views.clone_from(&stored.views);
        }
        for view in views.iter().rev() {
            stored.views.retain(|old| old.target != view.target);
            stored.views.insert(0, view.clone());
            stored.views.truncate(MAX_TARGETS);
            remember_workspace(&mut stored.workspace_views, view.clone());
        }
        let mut bytes = serde_json::to_vec(&stored)?;
        while bytes.len() as u64 > MAX_BYTES && stored.workspace_views.pop().is_some() {
            bytes = serde_json::to_vec(&stored)?;
        }
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "saved views exceed the size limit"
        );
        directory.replace_private_file(FILE, &bytes)
    }

    pub fn recorder(&self) -> Result<Recorder> {
        let (updates, mut incoming) = watch::channel(());
        let pending = Arc::new(Mutex::new(PendingViews::default()));
        let queued = pending.clone();
        let store = self.clone();
        let worker = thread::Builder::new()
            .name("corvid-view-save".into())
            .spawn(move || {
                let runtime = compio::runtime::Runtime::new()?;
                runtime.block_on(async {
                    while incoming.changed().await.is_ok() {
                        compio::time::sleep(SAVE_DELAY).await;
                        let views = std::mem::take(&mut queued.lock().unwrap().batch);
                        if !views.is_empty() {
                            store.save_views(&views)?;
                        }
                    }
                    Ok(())
                })
            })?;
        Ok(Recorder {
            updates: Some(updates),
            worker: Some(worker),
            pending,
        })
    }
}

fn remember_workspace(views: &mut Vec<SavedView>, view: SavedView) {
    views.retain(|old| !old.same_workspace(&view));
    views.insert(0, view);
    views.truncate(MAX_WORKSPACE_VIEWS);
}

#[derive(Default)]
struct PendingViews {
    latest: Option<SavedView>,
    batch: Vec<SavedView>,
}

pub struct Recorder {
    updates: Option<watch::Sender<()>>,
    worker: Option<thread::JoinHandle<Result<()>>>,
    pending: Arc<Mutex<PendingViews>>,
}

impl Recorder {
    pub fn record(&self, target: Target, host: &str, selection: &ViewSelection) {
        if let Ok(view) = SavedView::new(target, host, selection)
            && let Some(updates) = &self.updates
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.latest.as_ref() != Some(&view) {
                pending.latest = Some(view.clone());
                remember_workspace(&mut pending.batch, view);
                let _ = updates.send(());
            }
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
    let path =
        corvid_protocol::paths::state_directory(corvid_protocol::paths::StateRole::TuiViews, None)
            .ok()?;
    Some(ViewStore::new(path))
}

pub fn tui_runtime(override_path: Option<PathBuf>) -> Result<PathBuf> {
    corvid_protocol::paths::daemon_runtime(override_path, || {
        let view = tui_store()?.last()?;
        match view.target {
            Target::Tui { runtime } => Some(runtime),
            _ => None,
        }
    })?
    .prepare()
}

pub fn tui_target(runtime: &Path) -> Result<Target> {
    Ok(Target::Tui {
        runtime: std::path::absolute(runtime)?,
    })
}
