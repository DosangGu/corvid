use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail, ensure};
use corvid_core::layout::Layout;
use corvid_protocol::wire::HistoricalRun;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::history::{History, RunRecord};
use crate::platform::{StateDirectory, StateLock};

const STATE: &str = "state.json";
const BACKUP: &str = "state.json.bak";
const QUARANTINE: &str = "state.json.corrupt";
const SCHEMA_VERSION: u32 = 3;
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
}

impl Stored {
    fn from_layout(layout: Layout) -> Self {
        Self {
            layout: Arc::new(layout),
            history: Arc::new(History::default()),
        }
    }

    fn same_revision(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.layout, &other.layout) && Arc::ptr_eq(&self.history, &other.history)
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
    saved: watch::Receiver<Saved>,
    pub(crate) history: Arc<Vec<HistoricalRun>>,
    pub(crate) recovery: Recovery,
}

impl MetadataView {
    pub(crate) fn layout(&self) -> Arc<Layout> {
        Arc::clone(&self.updates.borrow().layout)
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

    #[cfg(target_os = "linux")]
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
pub(crate) struct MetadataUpdates(watch::Sender<Stored>);

impl MetadataUpdates {
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn in_memory(layout: Layout) -> Self {
        let (updates, _) = watch::channel(Stored::from_layout(layout));
        Self(updates)
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
        self.0.send_if_modified(|stored| {
            let mut changed = false;
            if let Some(layout) = layout {
                changed = true;
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
    let (results, saved) = watch::channel(Saved {
        stored,
        succeeded: true,
    });
    let task = tokio::spawn(write_loop(store, snapshots.clone(), results));
    Ok((
        MetadataView {
            updates: snapshots,
            saved,
            history,
            recovery,
        },
        MetadataUpdates(updates),
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
        2 | SCHEMA_VERSION => serde_json::from_slice::<Stored>(&bytes),
        version => bail!(
            "{name} uses unsupported layout schema {}; use a compatible Corvid version",
            version
        ),
    };
    match stored {
        Ok(stored) if stored.layout.validate().is_ok() && stored.history.validate() => {
            Ok(Candidate::Valid(stored, bytes))
        }
        _ => Ok(Candidate::Damaged(bytes)),
    }
}

fn encode(stored: &Stored) -> Result<Vec<u8>> {
    stored.layout.validate()?;
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
