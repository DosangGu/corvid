//! Desktop window membership and lifetime leases, independent of daemon authority.

use std::collections::HashSet;

use super::*;

const FILE: &str = "windows.json";
const BACKUP: &str = "windows.json.bak";
const MAX_WINDOWS: usize = 32;

#[cfg(windows)]
type Lease = std::fs::File;
#[cfg(any(target_os = "linux", target_os = "macos"))]
type Lease = nix::fcntl::Flock<std::fs::File>;

#[derive(Serialize, Deserialize)]
struct Windows {
    schema_version: u32,
    windows: Vec<Uuid>,
}

impl Default for Windows {
    fn default() -> Self {
        Self {
            schema_version: 1,
            windows: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct WindowRegistry {
    path: PathBuf,
}

/// Keep the lease until this window's recorder has flushed and its session stopped.
pub struct WindowState {
    pub id: Uuid,
    pub store: ViewStore,
    pub first_launch: bool,
    _lease: Lease,
}

impl WindowRegistry {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn restore(&self) -> Result<Vec<WindowState>> {
        let directory = retry_storage(|| Directory::create_all(&self.path))?;
        let _lock = retry_storage(|| directory.lock_storage())?;
        let existing = read(&directory)?;
        let from_empty_store = existing.is_none();
        let mut windows = existing.unwrap_or_default();
        let mut restored = Vec::new();
        for id in &windows.windows {
            if let Some(window) = self.claim(*id)? {
                restored.push(window);
            }
        }
        if from_empty_store {
            let mut window = self.allocate(&mut windows)?;
            window.first_launch = true;
            if let Some(legacy) = ViewStore::new(self.path.clone()).read() {
                for view in legacy.views.iter().rev() {
                    window.store.save(view)?;
                }
            }
            restored.push(window);
            write(&directory, &windows)?;
        }
        Ok(restored)
    }

    pub fn create(&self) -> Result<WindowState> {
        let directory = retry_storage(|| Directory::create_all(&self.path))?;
        let _lock = retry_storage(|| directory.lock_storage())?;
        let mut windows = read(&directory)?.unwrap_or_default();
        let window = self.allocate(&mut windows)?;
        write(&directory, &windows)?;
        Ok(window)
    }

    pub fn reclaim(&self, id: Uuid) -> Result<WindowState> {
        let directory = Directory::open(&self.path)?;
        let _lock = retry_storage(|| directory.lock_storage())?;
        ensure!(
            read(&directory)?.unwrap_or_default().windows.contains(&id),
            "the Desktop window was closed by another instance"
        );
        self.claim(id)?
            .context("another Desktop instance restored this window")
    }

    pub fn close(&self, window: WindowState) -> Result<()> {
        let directory = Directory::open(&self.path)?;
        let _lock = retry_storage(|| directory.lock_storage())?;
        let mut windows = read(&directory)?.unwrap_or_default();
        windows.windows.retain(|id| *id != window.id);
        write(&directory, &windows)?;
        let path = self.window_path(window.id);
        drop(window);
        // UUID-only descendants are created and owned by this registry. Holding the
        // registry lock prevents a concurrent restore from reopening this directory.
        std::fs::remove_dir_all(path).context("removing closed Desktop window preferences")
    }

    fn allocate(&self, windows: &mut Windows) -> Result<WindowState> {
        ensure!(
            windows.windows.len() < MAX_WINDOWS,
            "too many saved Desktop windows"
        );
        let id = Uuid::new_v4();
        let window = self
            .claim(id)?
            .context("could not reserve a new Desktop window")?;
        windows.windows.push(id);
        Ok(window)
    }

    fn window_path(&self, id: Uuid) -> PathBuf {
        self.path.join("windows").join(id.to_string())
    }

    fn claim(&self, id: Uuid) -> Result<Option<WindowState>> {
        let path = self.window_path(id);
        let directory = retry_storage(|| Directory::create_all(&path))?;
        let lease = match directory.lock_startup() {
            Ok(lease) => lease,
            Err(error) if locked(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(Some(WindowState {
            id,
            store: ViewStore {
                path,
                shared_workspace_path: Some(self.path.join("workspace-focus")),
            },
            first_launch: false,
            _lease: lease,
        }))
    }
}

fn locked(error: &anyhow::Error) -> bool {
    #[cfg(windows)]
    {
        error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.raw_os_error() == Some(32))
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        error
            .downcast_ref::<nix::errno::Errno>()
            .is_some_and(|error| *error == nix::errno::Errno::EWOULDBLOCK)
    }
}

fn decode_windows(bytes: &[u8]) -> Result<Windows> {
    let windows: Windows = serde_json::from_slice(bytes)?;
    ensure!(
        windows.schema_version == 1,
        "unsupported Desktop window schema"
    );
    ensure!(
        windows.windows.len() <= MAX_WINDOWS,
        "too many saved Desktop windows"
    );
    let unique: HashSet<_> = windows.windows.iter().collect();
    ensure!(
        unique.len() == windows.windows.len(),
        "duplicate Desktop window IDs"
    );
    Ok(windows)
}

fn read(directory: &Directory) -> Result<Option<Windows>> {
    super::backup_store::read_with_backup(
        directory,
        FILE,
        BACKUP,
        MAX_BYTES,
        decode_windows,
        require_supported_schema,
        "invalid Desktop window list",
    )
}

fn write(directory: &Directory, windows: &Windows) -> Result<()> {
    let _ = super::backup_store::load_for_write(
        directory,
        FILE,
        BACKUP,
        "windows.json.corrupt",
        MAX_BYTES,
        decode_windows,
        require_supported_schema,
    )?;
    directory.replace_private_file(FILE, &serde_json::to_vec(windows)?)
}

#[cfg(test)]
mod tests;
