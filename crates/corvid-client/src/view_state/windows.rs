//! Desktop window membership and lifetime leases, independent of daemon authority.

use std::collections::HashSet;

use super::*;

const FILE: &str = "windows.json";
const BACKUP: &str = "windows.json.bak";
const MAX_WINDOWS: usize = 32;

#[cfg(windows)]
type Lease = std::fs::File;
#[cfg(target_os = "linux")]
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
        let previous = directory.read_private_file(FILE, MAX_BYTES)?;
        let mut windows = read(&directory, previous.as_deref())?;
        let mut restored = Vec::new();
        for id in &windows.windows {
            if let Some(window) = self.claim(*id)? {
                restored.push(window);
            }
        }
        if previous.is_none() {
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
        let previous = directory.read_private_file(FILE, MAX_BYTES)?;
        let mut windows = read(&directory, previous.as_deref())?;
        let window = self.allocate(&mut windows)?;
        write(&directory, &windows)?;
        Ok(window)
    }

    pub fn reclaim(&self, id: Uuid) -> Result<WindowState> {
        let directory = Directory::open(&self.path)?;
        let _lock = retry_storage(|| directory.lock_storage())?;
        let previous = directory.read_private_file(FILE, MAX_BYTES)?;
        ensure!(
            read(&directory, previous.as_deref())?.windows.contains(&id),
            "the Desktop window was closed by another instance"
        );
        self.claim(id)?
            .context("another Desktop instance restored this window")
    }

    pub fn close(&self, window: WindowState) -> Result<()> {
        let directory = Directory::open(&self.path)?;
        let _lock = retry_storage(|| directory.lock_storage())?;
        let previous = directory.read_private_file(FILE, MAX_BYTES)?;
        let mut windows = read(&directory, previous.as_deref())?;
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
            store: ViewStore::new(path),
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
    #[cfg(target_os = "linux")]
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

fn read(directory: &Directory, bytes: Option<&[u8]>) -> Result<Windows> {
    let Some(bytes) = bytes else {
        return Ok(Windows::default());
    };
    match decode_windows(bytes) {
        Ok(windows) => Ok(windows),
        Err(_) => {
            require_supported_schema(bytes)?;
            directory
                .read_private_file(BACKUP, MAX_BYTES)?
                .map(|bytes| decode_windows(&bytes))
                .transpose()?
                .context("invalid Desktop window list")
        }
    }
}

fn write(directory: &Directory, windows: &Windows) -> Result<()> {
    if let Some(previous) = directory.read_private_file(FILE, MAX_BYTES)? {
        require_supported_schema(&previous)?;
        if decode_windows(&previous).is_ok() {
            directory.replace_private_file(BACKUP, &previous)?;
        } else {
            directory.replace_private_file("windows.json.corrupt", &previous)?;
        }
    }
    directory.replace_private_file(FILE, &serde_json::to_vec(windows)?)
}

#[cfg(test)]
mod tests;
