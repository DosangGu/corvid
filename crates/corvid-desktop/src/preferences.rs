use std::path::{Path, PathBuf};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use anyhow::Result;
#[cfg(unix)]
use corvid_protocol::unix::RuntimeDir as Directory;
#[cfg(windows)]
use corvid_protocol::windows::StateDirectory as Directory;
use serde::{Deserialize, Serialize};

const FILE: &str = "terminal.json";
const MAX_BYTES: u64 = 4096;
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(unix)]
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Preferences {
    pub(crate) font_family: Option<String>,
    #[serde(default)]
    pub(crate) allow_clipboard_write: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct PreferencesStore {
    path: PathBuf,
}

impl PreferencesStore {
    pub(crate) fn new(state: &Path) -> Self {
        Self {
            path: state.join("desktop-preferences"),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_dir(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) fn load(&self) -> Result<Preferences> {
        if self.path.try_exists()?
            && let Some(bytes) = Directory::open(&self.path)?.read_private_file(FILE, MAX_BYTES)?
        {
            return Ok(serde_json::from_slice(&bytes)?);
        }
        Ok(Preferences::default())
    }

    pub(crate) fn update(&self, mutate: impl FnOnce(&mut Preferences)) -> Result<Preferences> {
        let directory = Directory::create_all(&self.path)?;
        let _lock = lock_preferences(&directory)?;
        let mut preferences = if let Some(bytes) = directory.read_private_file(FILE, MAX_BYTES)? {
            serde_json::from_slice(&bytes)?
        } else {
            Preferences::default()
        };
        mutate(&mut preferences);
        directory.replace_private_file(FILE, &serde_json::to_vec(&preferences)?)?;
        Ok(preferences)
    }

    pub(crate) fn allows_clipboard_write(&self) -> bool {
        self.load()
            .map(|preferences| preferences.allow_clipboard_write)
            .unwrap_or_else(|error| {
                eprintln!("Could not read Desktop clipboard permission: {error:#}");
                false
            })
    }
}

#[cfg(windows)]
fn lock_preferences(directory: &Directory) -> Result<impl Drop + use<>> {
    directory.lock_storage_with_timeout(LOCK_TIMEOUT)
}

#[cfg(unix)]
fn lock_preferences(directory: &Directory) -> Result<impl Drop + use<>> {
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match directory.lock_storage() {
            Ok(lock) => return Ok(lock),
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
}
