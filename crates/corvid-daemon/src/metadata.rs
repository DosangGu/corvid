use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

use anyhow::{Result, bail, ensure};
use corvid_core::agent::AgentSessionKey;
use corvid_core::layout::Layout;
use corvid_core::layout::{CwdObservation, Location};
use corvid_protocol::wire::HistoricalRun;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::history::{History, RunRecord};
use crate::platform::{StateDirectory, StateLock};

const STATE: &str = "state.json";
const BACKUP: &str = "state.json.bak";
const QUARANTINE: &str = "state.json.corrupt";
const SCHEMA_VERSION: u32 = 5;
const MAX_BYTES: u64 = 16 * 1024 * 1024;
const DEBOUNCE: Duration = Duration::from_secs(1);

#[derive(Serialize)]
struct Save<'a> {
    schema_version: u32,
    #[serde(flatten)]
    stored: &'a Stored,
}

#[derive(Deserialize)]
struct Version {
    schema_version: u32,
}

#[derive(Deserialize)]
struct LegacyLayout {
    layout: Layout,
}

#[derive(Clone, Deserialize, Serialize)]
struct Stored {
    layout: Arc<Layout>,
    history: Arc<History>,
    #[serde(default)]
    resumes: Arc<BTreeMap<Uuid, SavedSession>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct SavedSession {
    pub(crate) key: AgentSessionKey,
    pub(crate) cwd: Option<CwdObservation>,
    pub(crate) verified: bool,
}

impl Stored {
    fn from_layout(layout: Layout) -> Self {
        Self {
            layout: Arc::new(layout),
            history: Arc::new(History::default()),
            resumes: Arc::default(),
        }
    }

    fn same_revision(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.layout, &other.layout)
            && Arc::ptr_eq(&self.history, &other.history)
            && Arc::ptr_eq(&self.resumes, &other.resumes)
    }
}

struct Saved {
    stored: Stored,
    succeeded: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum Recovery {
    Normal,
    Backup,
    Fresh,
}

#[derive(Clone, Copy)]
pub(crate) enum SaveState {
    Saved,
    Pending,
    Failed,
}

#[derive(Clone)]
pub(crate) struct MetadataView {
    updates: watch::Receiver<Stored>,
    display: watch::Receiver<Arc<Layout>>,
    saved: watch::Receiver<Saved>,
    pub(crate) history: Arc<Vec<HistoricalRun>>,
    pub(crate) recovery: Recovery,
}

impl MetadataView {
    pub(crate) fn display_layout(&self) -> Arc<Layout> {
        Arc::clone(&self.display.borrow())
    }

    pub(crate) fn save_state(&self) -> SaveState {
        let current = self.updates.borrow();
        let saved = self.saved.borrow();
        if !current.same_revision(&saved.stored) {
            SaveState::Pending
        } else if saved.succeeded {
            SaveState::Saved
        } else {
            SaveState::Failed
        }
    }

    pub(crate) async fn wait_saved(&mut self) -> bool {
        loop {
            match self.save_state() {
                SaveState::Saved => return true,
                SaveState::Failed => return false,
                SaveState::Pending => {}
            }
            if self.saved.changed().await.is_err() {
                return false;
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct MetadataUpdates(
    watch::Sender<Stored>,
    Arc<Mutex<HashMap<Uuid, String>>>,
    watch::Sender<Arc<Layout>>,
);

impl MetadataUpdates {
    pub(crate) fn saved_session(&self, pane: Uuid) -> Option<SavedSession> {
        self.0.borrow().resumes.get(&pane).cloned()
    }

    pub(crate) fn observe_session(&self, location: Location, key: Option<AgentSessionKey>) {
        self.0.send_if_modified(|stored| {
            if !stored.layout.contains(location) {
                return false;
            }
            let previous = stored.resumes.get(&location.pane);
            let next = match key.filter(AgentSessionKey::is_valid) {
                Some(key) => SavedSession {
                    key,
                    cwd: stored.layout.last_cwd(location).cloned(),
                    verified: true,
                },
                None => match previous {
                    Some(saved) if saved.verified => SavedSession {
                        verified: false,
                        ..saved.clone()
                    },
                    _ => return false,
                },
            };
            if previous == Some(&next) {
                return false;
            }
            Arc::make_mut(&mut stored.resumes).insert(location.pane, next);
            true
        });
    }

    fn new(updates: watch::Sender<Stored>) -> Self {
        let layout = Arc::new(updates.borrow().layout.display(&HashMap::new()));
        Self(updates, Arc::default(), watch::channel(layout).0)
    }

    fn refresh_display(&self) {
        self.2
            .send_replace(Arc::new(self.layout().display(&self.titles())));
    }

    pub(crate) fn titles(&self) -> HashMap<Uuid, String> {
        self.1
            .lock()
            .expect("terminal titles lock poisoned")
            .clone()
    }

    pub(crate) fn display_layout(&self) -> Arc<Layout> {
        Arc::clone(&self.2.borrow())
    }

    pub(crate) fn set_title(&self, pane: Uuid, title: String) -> bool {
        let mut titles = self.1.lock().expect("terminal titles lock poisoned");
        if titles.get(&pane).map(String::as_str).unwrap_or_default() == title {
            return false;
        }
        if title.is_empty() {
            titles.remove(&pane);
        } else {
            titles.insert(pane, title);
        }
        drop(titles);
        self.refresh_display();
        true
    }

    #[cfg(test)]
    pub(crate) fn in_memory(layout: Layout) -> Self {
        let (updates, _) = watch::channel(Stored::from_layout(layout));
        Self::new(updates)
    }

    pub(crate) fn layout(&self) -> Arc<Layout> {
        Arc::clone(&self.0.borrow().layout)
    }

    pub(crate) fn update(
        &self,
        layout: Option<Arc<Layout>>,
        history: impl Iterator<Item = RunRecord>,
    ) {
        let mut history = history.peekable();
        if layout.is_none() && history.peek().is_none() {
            return;
        }
        let update_display = layout.is_some();
        self.0.send_if_modified(|stored| {
            let mut changed = false;
            if let Some(layout) = layout {
                changed = true;
                let panes: std::collections::HashSet<_> =
                    layout.locations().map(|location| location.pane).collect();
                if stored.resumes.keys().any(|pane| !panes.contains(pane)) {
                    Arc::make_mut(&mut stored.resumes).retain(|pane, _| panes.contains(pane));
                }
                stored.layout = layout;
            }
            if history.peek().is_some() {
                // Retain the published revision when an observation only changes transient state.
                let mut next = Arc::clone(&stored.history);
                let records = Arc::make_mut(&mut next);
                let mut updated = false;
                for record in history {
                    updated |= records.record(record);
                }
                if updated {
                    stored.history = next;
                    changed = true;
                }
            }
            changed
        });
        if update_display {
            self.refresh_display();
        }
    }
}

pub(crate) fn start(
    directory: StateDirectory,
    lock: StateLock,
) -> Result<(MetadataView, MetadataUpdates, JoinHandle<()>)> {
    let (stored, bytes, recovery) = load(&directory)?;
    let history = Arc::new(stored.history.wire());
    let mut store = Store {
        directory,
        _lock: lock,
        last_good: bytes,
    };
    // Convert validated legacy files through the normal backup and replace path.
    if serde_json::from_slice::<Version>(&store.last_good)?.schema_version != SCHEMA_VERSION {
        store.save(&stored)?;
    }
    let (updates, snapshots) = watch::channel(stored.clone());
    let updates = MetadataUpdates::new(updates);
    let (results, saved) = watch::channel(Saved {
        stored,
        succeeded: true,
    });
    let task = tokio::spawn(write_loop(store, snapshots.clone(), results));
    Ok((
        MetadataView {
            updates: snapshots,
            display: updates.2.subscribe(),
            saved,
            history,
            recovery,
        },
        updates,
        task,
    ))
}

async fn write_loop(
    mut store: Store,
    mut updates: watch::Receiver<Stored>,
    saved: watch::Sender<Saved>,
) {
    while updates.changed().await.is_ok() {
        tokio::time::sleep(DEBOUNCE).await;
        let stored = updates.borrow_and_update().clone();
        let snapshot = stored.clone();
        let result = tokio::task::spawn_blocking(move || {
            let result = store.save(&snapshot);
            (store, result)
        })
        .await;
        match result {
            Ok((next_store, result)) => {
                store = next_store;
                if let Err(error) = &result {
                    eprintln!("corvid daemon: metadata save failed: {error:#}");
                }
                saved.send_replace(Saved {
                    stored,
                    succeeded: result.is_ok(),
                });
            }
            Err(_) => {
                saved.send_replace(Saved {
                    stored,
                    succeeded: false,
                });
                return;
            }
        }
    }
}

struct Store {
    directory: StateDirectory,
    _lock: StateLock,
    last_good: Vec<u8>,
}

impl Store {
    fn save(&mut self, stored: &Stored) -> Result<()> {
        let bytes = encode(stored)?;
        if bytes != self.last_good {
            match read(&self.directory, STATE)? {
                Candidate::Valid(_, current) if current == self.last_good => {}
                _ => bail!("layout file changed outside the daemon; restart to reload it"),
            }
            self.directory
                .replace_private_file(BACKUP, &self.last_good)?;
            self.directory.replace_private_file(STATE, &bytes)?;
            self.last_good = bytes;
        }
        Ok(())
    }
}

enum Candidate {
    Missing,
    Valid(Stored, Vec<u8>),
    Damaged(Vec<u8>),
}

fn load(directory: &StateDirectory) -> Result<(Stored, Vec<u8>, Recovery)> {
    let damaged = match read(directory, STATE)? {
        Candidate::Valid(layout, bytes) => return Ok((layout, bytes, Recovery::Normal)),
        Candidate::Damaged(bytes) => Some(bytes),
        Candidate::Missing => None,
    };

    let backup = read(directory, BACKUP)?;
    if let Some(bytes) = &damaged {
        directory.replace_private_file(QUARANTINE, bytes)?;
        eprintln!("corvid daemon: damaged layout retained in {QUARANTINE}");
    }
    if let Candidate::Valid(layout, bytes) = backup {
        directory.replace_private_file(STATE, &bytes)?;
        eprintln!("corvid daemon: restored layout from {BACKUP}");
        return Ok((layout, bytes, Recovery::Backup));
    }

    let stored = Stored::from_layout(Layout::empty_terminal());
    let bytes = encode(&stored)?;
    directory.replace_private_file(STATE, &bytes)?;
    let recovery = if damaged.is_some() || matches!(backup, Candidate::Damaged(_)) {
        eprintln!("corvid daemon: using a fresh empty layout");
        Recovery::Fresh
    } else {
        Recovery::Normal
    };
    Ok((stored, bytes, recovery))
}

fn read(directory: &StateDirectory, name: &str) -> Result<Candidate> {
    let Some(bytes) = directory.read_private_file(name, MAX_BYTES)? else {
        return Ok(Candidate::Missing);
    };
    let Ok(version) = serde_json::from_slice::<Version>(&bytes) else {
        return Ok(Candidate::Damaged(bytes));
    };
    let stored = match version.schema_version {
        1 => serde_json::from_slice::<LegacyLayout>(&bytes)
            .map(|old| Stored::from_layout(old.layout)),
        2 | 3 | 4 | SCHEMA_VERSION => serde_json::from_slice::<Stored>(&bytes),
        version => bail!(
            "{name} uses unsupported layout schema {}; use a compatible Corvid version",
            version
        ),
    };
    let stored = stored.map(|mut stored| {
        if version.schema_version < 5 {
            stored.resumes = Arc::default();
        }
        if version.schema_version < 4 {
            for workspace in &mut Arc::make_mut(&mut stored.layout).workspaces {
                for tab in &mut workspace.tabs {
                    let generated = tab.label == "Shell"
                        || tab
                            .label
                            .strip_prefix("Shell ")
                            .is_some_and(|suffix| suffix.parse::<usize>().is_ok());
                    tab.manual_name = (!generated).then(|| tab.label.clone());
                }
            }
        }
        stored
    });
    match stored {
        Ok(stored)
            if stored.layout.validate().is_ok()
                && stored.history.validate()
                && valid_resumes(&stored) =>
        {
            Ok(Candidate::Valid(stored, bytes))
        }
        _ => Ok(Candidate::Damaged(bytes)),
    }
}

fn encode(stored: &Stored) -> Result<Vec<u8>> {
    stored.layout.validate()?;
    ensure!(valid_resumes(stored), "invalid saved agent sessions");
    let mut bytes = serde_json::to_vec(&Save {
        schema_version: SCHEMA_VERSION,
        stored,
    })?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "metadata exceeds the size limit"
    );
    Ok(bytes)
}

fn valid_resumes(stored: &Stored) -> bool {
    let panes: std::collections::HashSet<_> = stored
        .layout
        .locations()
        .map(|location| location.pane)
        .collect();
    stored.resumes.iter().all(|(pane, saved)| {
        panes.contains(pane)
            && saved.key.is_valid()
            && saved.cwd.as_ref().is_none_or(|cwd| {
                !cwd.path.is_empty() && cwd.path.len() <= 4096 && !cwd.path.contains(&0)
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use corvid_core::agent::Provider;
    use corvid_core::layout::CloseTarget;

    fn key(id: &str) -> AgentSessionKey {
        AgentSessionKey {
            provider: Provider::Claude,
            provider_session_id: id.into(),
        }
    }

    #[test]
    fn saved_sessions_follow_pane_identity_and_are_removed_with_the_pane() {
        let mut layout = Layout::empty_terminal();
        let first = layout.primary().unwrap();
        let second = layout.add_tab(first.workspace, "Second".into()).unwrap();
        let metadata = MetadataUpdates::in_memory(layout);
        metadata.observe_session(first, Some(key("first")));
        metadata.observe_session(second, Some(key("second")));
        metadata.observe_session(first, Some(key("switched")));
        assert_eq!(
            metadata.saved_session(first.pane).unwrap().key,
            key("switched")
        );
        assert_eq!(
            metadata.saved_session(second.pane).unwrap().key,
            key("second")
        );
        metadata.observe_session(first, None);
        assert!(!metadata.saved_session(first.pane).unwrap().verified);
        let mut layout = metadata.layout();
        Arc::make_mut(&mut layout)
            .close(CloseTarget::Pane(first))
            .unwrap();
        metadata.update(Some(layout), std::iter::empty());
        assert!(metadata.saved_session(first.pane).is_none());
        assert!(metadata.saved_session(second.pane).is_some());
        // A late event from a deleted pane cannot recreate its resume entry.
        metadata.observe_session(first, Some(key("late")));
        assert!(metadata.saved_session(first.pane).is_none());
    }

    #[test]
    fn older_schemas_load_without_inventing_pane_sessions_and_newer_state_is_untouched() {
        let root = tempfile::tempdir_in(std::env::home_dir().unwrap()).unwrap();
        let directory = StateDirectory::create_all(&root.path().join("state")).unwrap();
        for schema in 1..=4 {
            let mut value =
                serde_json::to_value(Stored::from_layout(Layout::empty_terminal())).unwrap();
            value["schema_version"] = schema.into();
            let bytes = serde_json::to_vec(&value).unwrap();
            directory.replace_private_file(STATE, &bytes).unwrap();
            let Candidate::Valid(stored, _) = read(&directory, STATE).unwrap() else {
                panic!("legacy state rejected")
            };
            assert!(stored.resumes.is_empty());
            assert!(stored.layout.primary().is_some());
        }
        let bytes = br#"{"schema_version":99,"future":true}"#;
        directory.replace_private_file(STATE, bytes).unwrap();
        assert!(load(&directory).is_err());
        assert_eq!(
            directory
                .read_private_file(STATE, MAX_BYTES)
                .unwrap()
                .unwrap(),
            bytes
        );
    }
}
