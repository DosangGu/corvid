use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use corvid_client::{ControlEndpoint, SshAlias};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::platform::{StateDirectory, StateLock};
use crate::slots::{MAX_SLOTS, SlotLayout};

const CONFIG: &str = "config.json";
const SLOTS: &str = "slots.json";
const BACKUP: &str = "slots.json.bak";
const QUARANTINE: &str = "slots.json.corrupt";
const VERSION: u32 = 1;
const CONFIG_LIMIT: u64 = 4096;
const STATE_LIMIT: u64 = 2 * 1024 * 1024;

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
    layout: SlotLayout,
    last_good: Vec<u8>,
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
        let bytes = directory
            .read_private_file(CONFIG, CONFIG_LIMIT)?
            .context("companion config is missing")?;
        let version: Version = serde_json::from_slice(&bytes)
            .context("damaged companion config; repair it before starting")?;
        check_version(CONFIG, version)?;
        let config: Config = serde_json::from_slice(&bytes).context("invalid companion config")?;
        config.validate()?;
        let (layout, last_good, recovery) = load_slots(&directory, &config)?;
        Ok(Self {
            directory,
            _lock: lock,
            config,
            layout,
            last_good,
            recovery,
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn layout(&self) -> &SlotLayout {
        &self.layout
    }

    pub fn recovery(&self) -> Recovery {
        self.recovery
    }

    pub fn update<T>(&mut self, edit: impl FnOnce(&mut SlotLayout) -> Result<T>) -> Result<T> {
        let mut next = self.layout.clone();
        let result = edit(&mut next)?;
        if next == self.layout {
            return Ok(result);
        }
        ensure!(
            next.validate(self.config.slot_count, self.config.host_id),
            "invalid slot update"
        );
        let bytes = encode_layout(&next)?;
        let current = self.directory.read_private_file(SLOTS, STATE_LIMIT)?;
        ensure!(
            current.as_deref() == Some(&self.last_good),
            "slot file changed outside companion; restart to reload it"
        );
        self.directory
            .replace_private_file(BACKUP, &self.last_good)?;
        self.directory.replace_private_file(SLOTS, &bytes)?;
        self.layout = next;
        self.last_good = bytes;
        Ok(result)
    }
}

enum Candidate {
    Missing,
    Valid(SlotLayout, Vec<u8>),
    Damaged(Vec<u8>),
}

fn load_slots(
    directory: &StateDirectory,
    config: &Config,
) -> Result<(SlotLayout, Vec<u8>, Recovery)> {
    let damaged = match read_slots(directory, SLOTS, config)? {
        Candidate::Valid(layout, bytes) => return Ok((layout, bytes, Recovery::Normal)),
        Candidate::Damaged(bytes) => Some(bytes),
        Candidate::Missing => None,
    };
    let backup = read_slots(directory, BACKUP, config)?;
    if let Some(bytes) = &damaged {
        directory.replace_private_file(QUARANTINE, bytes)?;
    }
    if let Candidate::Valid(layout, bytes) = backup {
        directory.replace_private_file(SLOTS, &bytes)?;
        return Ok((layout, bytes, Recovery::Backup));
    }
    let layout = SlotLayout::new(config.slot_count)?;
    let bytes = encode_layout(&layout)?;
    directory.replace_private_file(SLOTS, &bytes)?;
    let recovery = if damaged.is_some() || matches!(backup, Candidate::Damaged(_)) {
        Recovery::Fresh
    } else {
        Recovery::Normal
    };
    Ok((layout, bytes, recovery))
}

fn read_slots(directory: &StateDirectory, name: &str, config: &Config) -> Result<Candidate> {
    let Some(bytes) = directory.read_private_file(name, STATE_LIMIT)? else {
        return Ok(Candidate::Missing);
    };
    let Ok(version) = serde_json::from_slice(&bytes) else {
        return Ok(Candidate::Damaged(bytes));
    };
    check_version(name, version)?;
    match serde_json::from_slice::<SavedSlots<SlotLayout>>(&bytes) {
        Ok(saved) if saved.layout.validate(config.slot_count, config.host_id) => {
            Ok(Candidate::Valid(saved.layout, bytes))
        }
        _ => Ok(Candidate::Damaged(bytes)),
    }
}

fn check_version(name: &str, version: Version) -> Result<()> {
    ensure!(
        version.schema_version == VERSION,
        "{name} uses unsupported schema {}; use a compatible Corvid version",
        version.schema_version
    );
    Ok(())
}

fn encode_layout(layout: &SlotLayout) -> Result<Vec<u8>> {
    encode(
        &SavedSlots {
            schema_version: VERSION,
            layout,
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
