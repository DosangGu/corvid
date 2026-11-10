use std::collections::BTreeMap;
#[cfg(windows)]
use std::io;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use corvid_client::{ControlEndpoint, SshAlias};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::last_known::LastKnown;
use crate::platform::{StateDirectory, StateLock};
use crate::slots::{MAX_SLOTS, RunKey, SlotLayout};

const CONFIG: &str = "config.json";
const SLOTS: &str = "slots.json";
const BACKUP: &str = "slots.json.bak";
const QUARANTINE: &str = "slots.json.corrupt";
const VERSION: u32 = 1;
const CONFIG_LIMIT: u64 = 4096;
const STATE_LIMIT: u64 = 2 * 1024 * 1024;
const MAX_CACHED_RUNS: usize = 512;

type StatusCache = BTreeMap<RunKey, LastKnown>;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostEndpoint {
    #[cfg(target_os = "linux")]
    Local {
        runtime_dir: PathBuf,
    },
    Ssh {
        alias: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    schema_version: u32,
    pub controller_id: Uuid,
    pub host_id: Uuid,
    pub endpoint: HostEndpoint,
    pub slot_count: u32,
}

impl Config {
    #[cfg(windows)]
    pub(crate) fn read(path: &Path) -> Result<Self> {
        Self::read_if_present(path)?.context("companion config is missing")
    }

    pub fn read_if_present(path: &Path) -> Result<Option<Self>> {
        let error = match StateDirectory::open(path) {
            Ok(directory) => return Self::read_from(&directory),
            Err(error) => error,
        };

        #[cfg(target_os = "linux")]
        let missing = error.downcast_ref::<nix::errno::Errno>() == Some(&nix::errno::Errno::ENOENT);
        #[cfg(windows)]
        let missing = error
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::NotFound);

        if missing {
            return Ok(None);
        }
        Err(error)
    }

    fn read_from(directory: &StateDirectory) -> Result<Option<Self>> {
        let Some(bytes) = directory.read_private_file(CONFIG, CONFIG_LIMIT)? else {
            return Ok(None);
        };
        let version: Version = serde_json::from_slice(&bytes)
            .context("damaged companion config; repair it before starting")?;
        check_version(CONFIG, version)?;
        let config: Self = serde_json::from_slice(&bytes).context("invalid companion config")?;
        config.validate()?;
        Ok(Some(config))
    }

    pub fn control_endpoint(&self) -> Result<ControlEndpoint> {
        Ok(match &self.endpoint {
            #[cfg(target_os = "linux")]
            HostEndpoint::Local { runtime_dir } => {
                ensure!(
                    runtime_dir.is_absolute(),
                    "daemon runtime directory must be absolute"
                );
                ControlEndpoint::Local(runtime_dir.clone())
            }
            HostEndpoint::Ssh { alias } => ControlEndpoint::Ssh(SshAlias::new(alias.clone())?),
        })
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.host_id.is_nil() && !self.controller_id.is_nil(),
            "invalid companion identity"
        );
        ensure!(
            (1..=MAX_SLOTS).contains(&self.slot_count),
            "invalid companion slot count"
        );
        self.control_endpoint()?;
        Ok(())
    }
}

#[derive(Deserialize)]
struct Version {
    schema_version: u32,
}

#[derive(Deserialize, Serialize)]
struct SavedSlots<T> {
    schema_version: u32,
    layout: T,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    last_known: Vec<CachedRun>,
}

#[derive(Deserialize, Serialize)]
struct CachedRun {
    key: RunKey,
    observation: LastKnown,
}

struct LoadedSlots {
    layout: SlotLayout,
    last_known: StatusCache,
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Recovery {
    Normal,
    Backup,
    Fresh,
}

pub struct Store {
    directory: StateDirectory,
    _lock: StateLock,
    config: Config,
    slots: LoadedSlots,
    recovery: Recovery,
}

pub fn initialize(
    path: &Path,
    host_id: Uuid,
    endpoint: HostEndpoint,
    slot_count: u32,
) -> Result<Uuid> {
    let config = Config {
        schema_version: VERSION,
        controller_id: Uuid::new_v4(),
        host_id,
        endpoint,
        slot_count,
    };
    config.validate()?;
    let bytes = encode(&config, CONFIG_LIMIT)?;
    let directory = StateDirectory::create_all(path)?;
    let _lock = directory.lock_storage()?;
    for file in [CONFIG, SLOTS, BACKUP, QUARANTINE] {
        ensure!(
            directory.read_private_file(file, STATE_LIMIT)?.is_none(),
            "companion state already exists; retain its identity or choose a fresh state directory"
        );
    }
    directory.replace_private_file(CONFIG, &bytes)?;
    Ok(config.controller_id)
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let directory =
            StateDirectory::open(path).context("open companion state created by companion init")?;
        let lock = directory.lock_storage()?;
        let config = Config::read_from(&directory)?.context("companion config is missing")?;
        let (slots, recovery) = load_slots(&directory, &config)?;
        Ok(Self {
            directory,
            _lock: lock,
            config,
            slots,
            recovery,
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn layout(&self) -> &SlotLayout {
        &self.slots.layout
    }

    pub fn recovery(&self) -> Recovery {
        self.recovery
    }

    pub fn update<T>(&mut self, edit: impl FnOnce(&mut SlotLayout) -> Result<T>) -> Result<T> {
        let mut next = self.slots.layout.clone();
        let result = edit(&mut next)?;
        if next == self.slots.layout {
            return Ok(result);
        }
        ensure!(
            next.validate(self.config.slot_count, self.config.host_id),
            "invalid slot update"
        );
        let last_known = collect_status(&next, |key| self.last_known(key));
        let bytes = self.write_slots(&next, &last_known)?;
        self.slots = LoadedSlots {
            layout: next,
            last_known,
            bytes,
        };
        Ok(result)
    }

    pub(crate) fn last_known(&self, key: RunKey) -> Option<LastKnown> {
        self.slots.last_known.get(&key).copied()
    }

    pub(crate) fn save_status(
        &mut self,
        mut observed: impl FnMut(RunKey) -> Option<LastKnown>,
    ) -> Result<()> {
        let last_known = collect_status(&self.slots.layout, |key| {
            observed(key).or_else(|| self.last_known(key))
        });
        if last_known != self.slots.last_known {
            self.slots.bytes = self.write_slots(&self.slots.layout, &last_known)?;
            self.slots.last_known = last_known;
        }
        Ok(())
    }

    fn write_slots(&self, layout: &SlotLayout, last_known: &StatusCache) -> Result<Vec<u8>> {
        let bytes = encode_slots(layout, last_known)?;
        let current = self.directory.read_private_file(SLOTS, STATE_LIMIT)?;
        ensure!(
            current.as_deref() == Some(&self.slots.bytes),
            "slot file changed outside companion; restart to reload it"
        );
        self.directory
            .replace_private_file(BACKUP, &self.slots.bytes)?;
        self.directory.replace_private_file(SLOTS, &bytes)?;
        Ok(bytes)
    }
}

enum Candidate {
    Missing,
    Valid(LoadedSlots),
    Damaged(Vec<u8>),
}

fn load_slots(directory: &StateDirectory, config: &Config) -> Result<(LoadedSlots, Recovery)> {
    let damaged = match read_slots(directory, SLOTS, config)? {
        Candidate::Valid(slots) => return Ok((slots, Recovery::Normal)),
        Candidate::Damaged(bytes) => Some(bytes),
        Candidate::Missing => None,
    };
    let backup = read_slots(directory, BACKUP, config)?;
    if let Some(bytes) = &damaged {
        directory.replace_private_file(QUARANTINE, bytes)?;
    }
    if let Candidate::Valid(slots) = backup {
        directory.replace_private_file(SLOTS, &slots.bytes)?;
        return Ok((slots, Recovery::Backup));
    }
    let layout = SlotLayout::new(config.slot_count)?;
    let last_known = StatusCache::new();
    let bytes = encode_slots(&layout, &last_known)?;
    directory.replace_private_file(SLOTS, &bytes)?;
    let recovery = if damaged.is_some() || matches!(backup, Candidate::Damaged(_)) {
        Recovery::Fresh
    } else {
        Recovery::Normal
    };
    Ok((
        LoadedSlots {
            layout,
            last_known,
            bytes,
        },
        recovery,
    ))
}

fn read_slots(directory: &StateDirectory, name: &str, config: &Config) -> Result<Candidate> {
    let Some(bytes) = directory.read_private_file(name, STATE_LIMIT)? else {
        return Ok(Candidate::Missing);
    };
    let Ok(version) = serde_json::from_slice(&bytes) else {
        return Ok(Candidate::Damaged(bytes));
    };
    check_version(name, version)?;
    let Ok(saved) = serde_json::from_slice::<SavedSlots<SlotLayout>>(&bytes) else {
        return Ok(Candidate::Damaged(bytes));
    };
    if !saved.layout.validate(config.slot_count, config.host_id)
        || saved.last_known.len() > MAX_CACHED_RUNS
    {
        return Ok(Candidate::Damaged(bytes));
    }
    let count = saved.last_known.len();
    let cached: StatusCache = saved
        .last_known
        .into_iter()
        .filter(|run| run.observation.is_valid())
        .map(|run| (run.key, run.observation))
        .collect();
    let last_known = collect_status(&saved.layout, |key| cached.get(&key).copied());
    if last_known.len() != count {
        return Ok(Candidate::Damaged(bytes));
    }
    Ok(Candidate::Valid(LoadedSlots {
        layout: saved.layout,
        last_known,
        bytes,
    }))
}

fn check_version(name: &str, version: Version) -> Result<()> {
    ensure!(
        version.schema_version == VERSION,
        "{name} uses unsupported schema {}; use a compatible Corvid version",
        version.schema_version
    );
    Ok(())
}

fn collect_status(
    layout: &SlotLayout,
    mut lookup: impl FnMut(RunKey) -> Option<LastKnown>,
) -> StatusCache {
    layout
        .slots()
        .iter()
        .filter_map(|slot| slot.run)
        .chain(layout.overflow())
        .filter_map(|key| lookup(key).map(|observation| (key, observation)))
        .take(MAX_CACHED_RUNS)
        .collect()
}

fn encode_slots(layout: &SlotLayout, last_known: &StatusCache) -> Result<Vec<u8>> {
    encode(
        &SavedSlots {
            schema_version: VERSION,
            layout,
            last_known: last_known
                .iter()
                .map(|(&key, &observation)| CachedRun { key, observation })
                .collect(),
        },
        STATE_LIMIT,
    )
}

fn encode(value: &impl Serialize, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() as u64 <= limit,
        "companion metadata exceeds size limit"
    );
    Ok(bytes)
}
