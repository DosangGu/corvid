use std::fs::{self, File, Metadata, Permissions};
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg, OFlag, openat};
use nix::sys::stat::{Mode, mkdirat};
use nix::unistd::geteuid;
use tempfile::NamedTempFile;
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;

const DIRECTORY_FLAGS: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_DIRECTORY)
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_CLOEXEC);
const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketRole {
    Client,
    Control,
    Collector,
    Presentation,
    Upgrade,
    TerminalHost,
}

impl SocketRole {
    fn name(self) -> &'static str {
        match self {
            Self::Client => "client.sock",
            Self::Control => "control.sock",
            Self::Collector => "collector.sock",
            Self::Presentation => "companion.sock",
            Self::Upgrade => "upgrade.sock",
            Self::TerminalHost => "pty.sock",
        }
    }
}

pub struct RuntimeDir {
    path: PathBuf,
    directory: File,
    uid: u32,
}

impl RuntimeDir {
    pub fn open(path: &Path) -> Result<Self> {
        Self::validate(path, Create::None)
    }

    pub fn create(path: &Path) -> Result<Self> {
        Self::validate(path, Create::Final)
    }

    pub fn create_all(path: &Path) -> Result<Self> {
        Self::validate(path, Create::Parents)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn validate(path: &Path, create: Create) -> Result<Self> {
        let uid = geteuid().as_raw();
        let directory = open_runtime_path(path, uid, create)?;
        Ok(Self {
            path: path.to_owned(),
            directory,
            uid,
        })
    }

    pub fn lock_instance(&self) -> Result<Instance<'_>> {
        Ok(Instance {
            runtime: self,
            _lock: self.lock_file("daemon.lock")?,
        })
    }

    pub fn lock_startup(&self) -> Result<Flock<File>> {
        self.lock_file("startup.lock")
    }

    pub fn lock_storage(&self) -> Result<Flock<File>> {
        self.lock_file("state.lock")
    }

    pub fn read_private_file(&self, name: &str, max_bytes: u64) -> Result<Option<Vec<u8>>> {
        let _ = self.private_file_path(name)?;
        let fd = match openat(
            &self.directory,
            name,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::ENOENT) => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("opening {name}")),
        };
        let file = File::from(fd);
        let metadata = file.metadata()?;
        self.check_private_file(name, &metadata)?;
        ensure!(metadata.len() <= max_bytes, "{name} exceeds its size limit");
        let mut bytes = Vec::new();
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= max_bytes,
            "{name} exceeds its size limit"
        );
        Ok(Some(bytes))
    }

    pub fn replace_private_file(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let destination = self.private_file_path(name)?;
        match fs::symlink_metadata(&destination) {
            Ok(metadata) => self.check_private_file(name, &metadata)?,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("examining {name}")),
        }
        let mut temporary = NamedTempFile::new_in(&self.path)?;
        temporary.write_all(bytes)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(destination)
            .with_context(|| format!("publishing {name}"))?;
        self.directory.sync_all()?;
        Ok(())
    }

    fn private_file_path(&self, name: &str) -> Result<PathBuf> {
        let path = Path::new(name);
        ensure!(
            path.file_name() == Some(name.as_ref()),
            "invalid private file name"
        );
        Ok(self.path.join(path))
    }

    fn check_private_file(&self, name: &str, metadata: &Metadata) -> Result<()> {
        ensure!(
            metadata.is_file() && metadata.uid() == self.uid && metadata.mode() & 0o777 == 0o600,
            "{name} must be a private regular file owned by this UID"
        );
        Ok(())
    }

    fn lock_file(&self, name: &str) -> Result<Flock<File>> {
        let fd = openat(
            &self.directory,
            name,
            OFlag::O_RDWR
                | OFlag::O_CREAT
                | OFlag::O_NOFOLLOW
                | OFlag::O_CLOEXEC
                | OFlag::O_NONBLOCK,
            Mode::from_bits_truncate(0o600),
        )
        .context("opening Corvid lock")?;
        let file = File::from(fd);
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == self.uid
                && metadata.mode() & 0o777 == 0o600
                && metadata.nlink() == 1,
            "unsafe Corvid lock"
        );

        Flock::lock(file, FlockArg::LockExclusiveNonblock)
            .map_err(|(_, error)| error)
            .with_context(|| format!("acquiring {name}"))
    }

    pub async fn connect(&self, role: SocketRole) -> Result<UnixStream> {
        let path = self.socket_path(role);
        let metadata = fs::symlink_metadata(&path).context("opening daemon socket")?;
        check_socket(&metadata, self.uid)?;

        let stream = timeout(SOCKET_PROBE_TIMEOUT, UnixStream::connect(&path))
            .await
            .context("connecting to daemon socket timed out")?
            .context("connecting to daemon socket")?;
        authenticate(&stream, self.uid)?;
        Ok(stream)
    }

    fn socket_path(&self, role: SocketRole) -> PathBuf {
        if matches!(role, SocketRole::TerminalHost) {
            // Address the open private directory without extending the runtime pathname.
            return PathBuf::from(format!(
                "/proc/self/fd/{}/{}",
                self.directory.as_raw_fd(),
                role.name()
            ));
        }
        self.path.join(role.name())
    }
}

impl AsFd for RuntimeDir {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.directory.as_fd()
    }
}

#[derive(Clone, Copy)]
enum Create {
    None,
    Final,
    Parents,
}

fn open_runtime_path(path: &Path, uid: u32, create: Create) -> Result<File> {
    let mut components = path.components().peekable();
    ensure!(
        matches!(components.next(), Some(Component::RootDir)) && components.peek().is_some(),
        "runtime directory must be an absolute path below the root"
    );

    let mut parent = File::open("/")?;
    check_parent(&parent.metadata()?, uid)?;
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            bail!("runtime directory cannot contain parent traversal");
        };
        let is_final = components.peek().is_none();
        let may_create =
            matches!(create, Create::Parents) || (is_final && matches!(create, Create::Final));
        parent = open_component(&parent, name, may_create)?;
        let metadata = parent.metadata()?;
        if is_final {
            ensure!(
                metadata.uid() == uid && metadata.mode() & 0o777 == 0o700,
                "runtime directory must belong to this UID and have mode 0700"
            );
        } else {
            check_parent(&metadata, uid)?;
        }
    }
    Ok(parent)
}

fn open_component(parent: &File, name: &std::ffi::OsStr, create: bool) -> Result<File> {
    let fd = match openat(parent, name, DIRECTORY_FLAGS, Mode::empty()) {
        Err(Errno::ENOENT) if create => {
            match mkdirat(parent, name, Mode::from_bits_truncate(0o700)) {
                Ok(()) | Err(Errno::EEXIST) => {}
                Err(error) => return Err(error).context("creating private directory"),
            }
            openat(parent, name, DIRECTORY_FLAGS, Mode::empty())
        }
        result => result,
    }
    .context("opening private path without following symlinks")?;
    Ok(File::from(fd))
}

fn check_parent(metadata: &Metadata, uid: u32) -> Result<()> {
    ensure!(
        (metadata.uid() == 0 || metadata.uid() == uid) && metadata.mode() & 0o022 == 0,
        "runtime parent has an unsafe owner or writable permissions"
    );
    Ok(())
}

fn check_socket(metadata: &Metadata, uid: u32) -> Result<()> {
    ensure!(
        metadata.file_type().is_socket()
            && metadata.uid() == uid
            && metadata.mode() & 0o777 == 0o600,
        "daemon socket must be owned by this UID and have mode 0600"
    );
    Ok(())
}

fn authenticate(stream: &UnixStream, uid: u32) -> Result<()> {
    let peer = stream
        .peer_cred()
        .context("reading Unix peer credentials")?;
    ensure!(peer.uid() == uid, "unauthorized Unix peer UID");
    Ok(())
}

pub struct Instance<'a> {
    runtime: &'a RuntimeDir,
    _lock: Flock<File>,
}

impl Instance<'_> {
    pub async fn bind(&self, role: SocketRole) -> Result<BoundSocket<'_>> {
        let path = self.runtime.path.join(role.name());
        let socket_path = self.runtime.socket_path(role);
        remove_stale(&socket_path, self.runtime.uid).await?;
        let listener = UnixListener::bind(&socket_path).context("binding daemon socket")?;
        let metadata = fs::symlink_metadata(&path).context("reading bound socket identity")?;
        let socket = BoundSocket {
            instance: self,
            listener,
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        fs::set_permissions(&socket.path, Permissions::from_mode(0o600))
            .context("protecting daemon socket")?;
        Ok(socket)
    }
}

async fn remove_stale(path: &Path, uid: u32) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("examining existing socket path"),
    };
    check_socket(&metadata, uid)?;

    let result = timeout(SOCKET_PROBE_TIMEOUT, UnixStream::connect(path)).await;
    match result {
        Ok(Err(error)) if error.kind() == ErrorKind::ConnectionRefused => {
            unlink_if_same(path, metadata.dev(), metadata.ino())?;
            Ok(())
        }
        Ok(Err(error)) if error.kind() == ErrorKind::NotFound => Ok(()),
        _ => bail!("socket may belong to a live daemon"),
    }
}

fn unlink_if_same(path: &Path, device: u64, inode: u64) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_socket()
                && metadata.dev() == device
                && metadata.ino() == inode =>
        {
            fs::remove_file(path)
        }
        Ok(_) => Err(io::Error::other("socket path identity changed")),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub struct BoundSocket<'a> {
    instance: &'a Instance<'a>,
    listener: UnixListener,
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl BoundSocket<'_> {
    /// A rejected peer yields `None`. Listener errors remain separate.
    pub async fn accept(&self) -> Result<Option<UnixStream>> {
        let (stream, _) = self
            .listener
            .accept()
            .await
            .context("accepting Unix peer")?;
        if authenticate(&stream, self.instance.runtime.uid).is_err() {
            return Ok(None);
        }
        Ok(Some(stream))
    }
}

impl Drop for BoundSocket<'_> {
    fn drop(&mut self) {
        let _ = unlink_if_same(&self.path, self.device, self.inode);
    }
}
