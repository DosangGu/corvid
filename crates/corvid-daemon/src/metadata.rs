use std::fs::File;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail, ensure};
use corvid_core::layout::Layout;
use corvid_protocol::unix::RuntimeDir;
use nix::fcntl::Flock;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::task::JoinHandle;

const STATE: &str = "state.json";
const BACKUP: &str = "state.json.bak";
const QUARANTINE: &str = "state.json.corrupt";
const SCHEMA_VERSION: u32 = 1;
const MAX_BYTES: u64 = 16 * 1024 * 1024;
const DEBOUNCE: Duration = Duration::from_secs(1);

#[derive(Serialize)]
struct Save<'a> {
    schema_version: u32,
    layout: &'a Layout,
}

#[derive(Deserialize)]
struct Version {
    schema_version: u32,
}

#[derive(Deserialize)]
struct Stored {
    layout: Layout,
}

struct Saved {
    layout: Arc<Layout>,
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
    layouts: watch::Receiver<Arc<Layout>>,
    saved: watch::Receiver<Saved>,
    pub(crate) recovery: Recovery,
}

impl MetadataView {
    pub(crate) fn layout(&self) -> Arc<Layout> {
        Arc::clone(&self.layouts.borrow())
    }

    pub(crate) fn save_state(&self) -> SaveState {
        let current = self.layouts.borrow();
        let saved = self.saved.borrow();
        if !Arc::ptr_eq(&saved.layout, &current) {
            SaveState::Pending
        } else if saved.succeeded {
            SaveState::Saved
        } else {
            SaveState::Failed
        }
    }
}

pub(crate) fn start(
    directory: RuntimeDir,
) -> Result<(MetadataView, watch::Sender<Arc<Layout>>, JoinHandle<()>)> {
    let lock = directory.lock_storage()?;
    let (layout, bytes, recovery) = load(&directory)?;
    let layout = Arc::new(layout);
    let store = Store {
        directory,
        _lock: lock,
        last_good: bytes,
    };
    let (updates, layouts) = watch::channel(Arc::clone(&layout));
    let (results, saved) = watch::channel(Saved {
        layout,
        succeeded: true,
    });
    let task = tokio::spawn(write_loop(store, layouts.clone(), results));
    Ok((
        MetadataView {
            layouts,
            saved,
            recovery,
        },
        updates,
        task,
    ))
}

async fn write_loop(
    mut store: Store,
    mut layouts: watch::Receiver<Arc<Layout>>,
    saved: watch::Sender<Saved>,
) {
    while layouts.changed().await.is_ok() {
        tokio::time::sleep(DEBOUNCE).await;
        let layout = Arc::clone(&layouts.borrow_and_update());
        let snapshot = Arc::clone(&layout);
        let result = tokio::task::spawn_blocking(move || {
            let result = store.save(&snapshot);
            (store, result)
        })
        .await;
        match result {
            Ok((next_store, result)) => {
                store = next_store;
                if let Err(error) = &result {
                    eprintln!("corvid daemon: layout save failed: {error:#}");
                }
                saved.send_replace(Saved {
                    layout,
                    succeeded: result.is_ok(),
                });
            }
            Err(_) => {
                saved.send_replace(Saved {
                    layout,
                    succeeded: false,
                });
                return;
            }
        }
    }
}

struct Store {
    directory: RuntimeDir,
    _lock: Flock<File>,
    last_good: Vec<u8>,
}

impl Store {
    fn save(&mut self, layout: &Layout) -> Result<()> {
        let bytes = encode(layout)?;
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
    Valid(Layout, Vec<u8>),
    Damaged(Vec<u8>),
}

fn load(directory: &RuntimeDir) -> Result<(Layout, Vec<u8>, Recovery)> {
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

    let layout = Layout::empty_terminal();
    let bytes = encode(&layout)?;
    directory.replace_private_file(STATE, &bytes)?;
    let recovery = if damaged.is_some() || matches!(backup, Candidate::Damaged(_)) {
        eprintln!("corvid daemon: using a fresh empty layout");
        Recovery::Fresh
    } else {
        Recovery::Normal
    };
    Ok((layout, bytes, recovery))
}

fn read(directory: &RuntimeDir, name: &str) -> Result<Candidate> {
    let Some(bytes) = directory.read_private_file(name, MAX_BYTES)? else {
        return Ok(Candidate::Missing);
    };
    let Ok(version) = serde_json::from_slice::<Version>(&bytes) else {
        return Ok(Candidate::Damaged(bytes));
    };
    if version.schema_version != SCHEMA_VERSION {
        bail!(
            "{name} uses unsupported layout schema {}; use a compatible Corvid version",
            version.schema_version
        );
    }
    match serde_json::from_slice::<Stored>(&bytes) {
        Ok(stored) if stored.layout.validate().is_ok() => {
            Ok(Candidate::Valid(stored.layout, bytes))
        }
        _ => Ok(Candidate::Damaged(bytes)),
    }
}

fn encode(layout: &Layout) -> Result<Vec<u8>> {
    layout.validate()?;
    let mut bytes = serde_json::to_vec(&Save {
        schema_version: SCHEMA_VERSION,
        layout,
    })?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "layout exceeds the size limit"
    );
    Ok(bytes)
}
