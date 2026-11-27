use std::fs::{self, File, Metadata, Permissions};
use std::io::{self, ErrorKind, Read, Write};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
#[cfg(feature = "compio")]
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg, OFlag, openat};
use nix::sys::stat::{Mode, mkdirat};
use nix::unistd::geteuid;
use tempfile::NamedTempFile;

const DIRECTORY_FLAGS: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_DIRECTORY)
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_CLOEXEC);
#[cfg(feature = "compio")]
const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Best-effort path lookup for local launch/upgrade clients. This PID-only
/// query is not process-identity or ownership evidence; the daemon uses bound
/// observation handles and verifies birth/executable identity separately.
pub fn process_image_path(pid: u32) -> io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        fs::read_link(format!("/proc/{pid}/exe"))
    }
    #[cfg(target_os = "macos")]
    {
        crate::macos::process_path(pid)
    }
}

#[cfg(target_os = "macos")]
fn validate_socket_path(path: &Path) -> Result<()> {
    std::os::unix::net::SocketAddr::from_pathname(path).context("invalid Unix socket path")?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketRole {
    Client,
    Control,
    Collector,
    Presentation,
    Upgrade,
    TerminalHost,
}

#[cfg(feature = "compio")]
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
        #[cfg(target_os = "macos")]
        macos_acl::check_private(file.as_fd(), name)?;
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
            Ok(metadata) => {
                self.check_private_file(name, &metadata)?;
                #[cfg(target_os = "macos")]
                macos_acl::check_private_path(&destination, name)?;
            }
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
        #[cfg(target_os = "macos")]
        macos_acl::check_private(file.as_fd(), name)?;

        Flock::lock(file, FlockArg::LockExclusiveNonblock)
            .map_err(|(_, error)| error)
            .with_context(|| format!("acquiring {name}"))
    }

    #[cfg(feature = "compio")]
    pub async fn connect(&self, role: SocketRole) -> Result<compio::net::UnixStream> {
        self.connect_compio(role).await
    }

    #[cfg(feature = "compio")]
    pub async fn connect_compio(&self, role: SocketRole) -> Result<compio::net::UnixStream> {
        let path = self.checked_socket_path(role)?;
        let stream = compio::time::timeout(
            SOCKET_PROBE_TIMEOUT,
            compio::net::UnixStream::connect(&path),
        )
        .await
        .context("connecting to daemon socket timed out")?
        .context("connecting to daemon socket")?;
        authenticate(&stream, self.uid)?;
        Ok(stream)
    }

    #[cfg(feature = "compio")]
    fn checked_socket_path(&self, role: SocketRole) -> Result<PathBuf> {
        let path = self.socket_path(role)?;
        let metadata = fs::symlink_metadata(&path).context("opening daemon socket")?;
        check_socket(&metadata, self.uid)?;
        Ok(path)
    }

    #[cfg(feature = "compio")]
    fn socket_path(&self, role: SocketRole) -> Result<PathBuf> {
        #[cfg(target_os = "linux")]
        if matches!(role, SocketRole::TerminalHost) {
            // Address the open private directory without extending the runtime pathname.
            return Ok(PathBuf::from(format!(
                "/proc/self/fd/{}/{}",
                self.directory.as_raw_fd(),
                role.name()
            )));
        }
        let path = self.path.join(role.name());
        #[cfg(target_os = "macos")]
        validate_socket_path(&path)?;
        Ok(path)
    }
}

/// Obtain the kernel-authenticated process ID, never a client-provided label.
pub fn peer_pid(stream: &impl AsFd) -> io::Result<u32> {
    use nix::sys::socket::{getsockopt, sockopt};

    #[cfg(target_os = "linux")]
    let pid = getsockopt(stream, sockopt::PeerCredentials)?.pid();
    #[cfg(target_os = "macos")]
    let pid = getsockopt(stream, sockopt::LocalPeerPid)?;
    u32::try_from(pid)
        .ok()
        .filter(|pid| *pid != 0)
        .ok_or_else(|| io::Error::other("invalid peer PID"))
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
            #[cfg(target_os = "macos")]
            macos_acl::check_private(parent.as_fd(), "runtime directory")?;
        } else {
            check_parent(&metadata, uid)?;
            #[cfg(target_os = "macos")]
            macos_acl::check_parent(parent.as_fd())?;
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

#[cfg(feature = "compio")]
fn check_socket(metadata: &Metadata, uid: u32) -> Result<()> {
    ensure!(
        metadata.file_type().is_socket()
            && metadata.uid() == uid
            && metadata.mode() & 0o777 == 0o600,
        "daemon socket must be owned by this UID and have mode 0600"
    );
    Ok(())
}

#[cfg(feature = "compio")]
fn authenticate(stream: &impl AsFd, uid: u32) -> Result<()> {
    #[cfg(target_os = "linux")]
    let peer_uid = nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials)
        .context("reading Unix peer credentials")?
        .uid();
    #[cfg(target_os = "macos")]
    let peer_uid = nix::unistd::getpeereid(stream)
        .context("reading Unix peer credentials")?
        .0
        .as_raw();
    ensure!(peer_uid == uid, "unauthorized Unix peer UID");
    Ok(())
}

pub struct Instance<'a> {
    runtime: &'a RuntimeDir,
    _lock: Flock<File>,
}

impl Instance<'_> {
    #[cfg(feature = "compio")]
    pub async fn bind(&self, role: SocketRole) -> Result<CompioBoundSocket<'_>> {
        self.bind_compio(role).await
    }

    #[cfg(feature = "compio")]
    pub async fn bind_compio(&self, role: SocketRole) -> Result<CompioBoundSocket<'_>> {
        let socket_path = self.runtime.socket_path(role)?;
        remove_stale(&socket_path, self.runtime.uid, async {
            compio::time::timeout(SOCKET_PROBE_TIMEOUT, async {
                loop {
                    // A pending completion-based accept can keep a socket alive
                    // briefly after its previous daemon exits. Only remove it
                    // once the kernel confirms it no longer takes connections.
                    match compio::net::UnixStream::connect(&socket_path).await {
                        Ok(stream) => drop(stream),
                        Err(error) => return Err::<(), _>(error),
                    }
                    compio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .map_err(|_| ())
        })
        .await?;
        // Publish only after setting the socket mode. The staged pathname is
        // shorter than every public name so it also fits on macOS.
        let staging = tempfile::Builder::new()
            .prefix(".")
            .rand_bytes(4)
            .tempdir_in(socket_path.parent().context("socket has no parent")?)?;
        let staged = staging.path().join("s");
        let listener = compio::net::UnixListener::bind(&staged)
            .await
            .context("binding daemon socket")?;
        fs::set_permissions(&staged, Permissions::from_mode(0o600))
            .context("protecting daemon socket")?;
        let metadata = fs::symlink_metadata(&staged).context("reading bound socket identity")?;
        rustix::fs::renameat_with(
            rustix::fs::CWD,
            &staged,
            rustix::fs::CWD,
            &socket_path,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .context("publishing daemon socket")?;
        Ok(SocketListener {
            instance: self,
            listener,
            path: self.runtime.path.join(role.name()),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

#[cfg(feature = "compio")]
async fn remove_stale<S>(
    path: &Path,
    uid: u32,
    connect: impl Future<Output = std::result::Result<io::Result<S>, ()>>,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("examining existing socket path"),
    };
    check_socket(&metadata, uid)?;

    match connect.await {
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

pub struct SocketListener<'a, L> {
    instance: &'a Instance<'a>,
    listener: L,
    path: PathBuf,
    device: u64,
    inode: u64,
}

#[cfg(feature = "compio")]
pub type CompioBoundSocket<'a> = SocketListener<'a, compio::net::UnixListener>;

#[cfg(feature = "compio")]
impl CompioBoundSocket<'_> {
    /// A rejected peer yields `None`. Listener errors remain separate.
    pub async fn accept(&self) -> Result<Option<compio::net::UnixStream>> {
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

impl<L> Drop for SocketListener<'_, L> {
    fn drop(&mut self) {
        let _ = unlink_if_same(&self.path, self.device, self.inode);
    }
}

#[cfg(target_os = "macos")]
mod macos_acl {
    use std::ffi::{CString, c_int, c_void};
    use std::io;
    use std::os::fd::{AsRawFd, BorrowedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use anyhow::{Context, Result, bail};

    type AclT = *mut c_void;
    type AclEntryT = *mut c_void;
    type AclPermsetT = *mut c_void;
    type AclTagT = u32;
    type AclTypeT = u32;
    type AclPermT = u32;

    const ACL_TYPE_EXTENDED: AclTypeT = 0x0000_0100;
    const ACL_FIRST_ENTRY: c_int = 0;
    const ACL_NEXT_ENTRY: c_int = -1;
    const ACL_EXTENDED_ALLOW: AclTagT = 1;
    const ACL_EXTENDED_DENY: AclTagT = 2;

    // macOS <sys/acl.h> write/modify permissions that let another account alter
    // directory contents, metadata, or ACLs on an ancestor directory.
    const WRITE_PERMS: &[AclPermT] = &[
        1 << 2,  // ACL_WRITE_DATA / ACL_ADD_FILE
        1 << 4,  // ACL_APPEND_DATA / ACL_ADD_SUBDIRECTORY
        1 << 7,  // ACL_DELETE
        1 << 9,  // ACL_DELETE_CHILD
        1 << 10, // ACL_WRITE_ATTRIBUTES
        1 << 12, // ACL_WRITE_EXTATTRIBUTES
        1 << 14, // ACL_WRITE_SECURITY
        1 << 15, // ACL_CHANGE_OWNER
    ];

    unsafe extern "C" {
        fn acl_get_fd_np(fd: c_int, acl_type: AclTypeT) -> AclT;
        fn acl_get_link_np(path_p: *const std::ffi::c_char, acl_type: AclTypeT) -> AclT;
        fn acl_get_entry(acl: AclT, entry_id: c_int, entry_p: *mut AclEntryT) -> c_int;
        fn acl_get_tag_type(entry_d: AclEntryT, tag_type_p: *mut AclTagT) -> c_int;
        fn acl_get_permset(entry_d: AclEntryT, permset_p: *mut AclPermsetT) -> c_int;
        fn acl_get_perm_np(permset_d: AclPermsetT, perm: AclPermT) -> c_int;
        fn acl_free(obj_p: *mut c_void) -> c_int;
    }

    struct AclGuard(AclT);

    impl Drop for AclGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` was returned by `acl_get_fd_np` or `acl_get_link_np`.
                unsafe {
                    acl_free(self.0);
                }
            }
        }
    }

    pub(super) fn check_private(fd: BorrowedFd<'_>, context: &str) -> Result<()> {
        let Some(acl) = fetch_fd_acl(fd, context)? else {
            return Ok(());
        };
        inspect_entries(acl.0, false, context)
    }

    pub(super) fn check_private_path(path: &Path, context: &str) -> Result<()> {
        let c_path = CString::new(path.as_os_str().as_bytes())
            .with_context(|| format!("invalid path for {context}"))?;
        // SAFETY: `c_path` is NUL-terminated; `ACL_TYPE_EXTENDED` is valid on macOS.
        let raw = unsafe { acl_get_link_np(c_path.as_ptr(), ACL_TYPE_EXTENDED) };
        let Some(acl) = wrap_acl(raw, context)? else {
            return Ok(());
        };
        inspect_entries(acl.0, false, context)
    }

    pub(super) fn check_parent(fd: BorrowedFd<'_>) -> Result<()> {
        let Some(acl) = fetch_fd_acl(fd, "runtime parent")? else {
            return Ok(());
        };
        inspect_entries(acl.0, true, "runtime parent")
    }

    fn fetch_fd_acl(fd: BorrowedFd<'_>, context: &str) -> Result<Option<AclGuard>> {
        // SAFETY: `fd` is a valid open file descriptor and `ACL_TYPE_EXTENDED` is valid on macOS.
        let raw = unsafe { acl_get_fd_np(fd.as_raw_fd(), ACL_TYPE_EXTENDED) };
        wrap_acl(raw, context)
    }

    fn wrap_acl(raw: AclT, context: &str) -> Result<Option<AclGuard>> {
        if raw.is_null() {
            let error = io::Error::last_os_error();
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOENT | libc::ENOTSUP | libc::EOPNOTSUPP)
            ) {
                return Ok(None);
            }
            return Err(error).with_context(|| format!("reading macOS ACL for {context}"));
        }
        Ok(Some(AclGuard(raw)))
    }

    fn inspect_entries(acl: AclT, allow_read_only: bool, context: &str) -> Result<()> {
        let mut entry: AclEntryT = std::ptr::null_mut();
        let mut which = ACL_FIRST_ENTRY;
        loop {
            // SAFETY: `acl` is a live `acl_t` owned by `AclGuard` and `entry` is writable.
            let status = unsafe { acl_get_entry(acl, which, &mut entry) };
            which = ACL_NEXT_ENTRY;
            if status == -1 {
                break;
            }
            if status != 0 {
                return Err(io::Error::last_os_error())
                    .with_context(|| format!("iterating macOS ACL for {context}"));
            }
            let mut tag: AclTagT = 0;
            // SAFETY: `entry` was populated by `acl_get_entry`.
            if unsafe { acl_get_tag_type(entry, &mut tag) } != 0 {
                return Err(io::Error::last_os_error())
                    .with_context(|| format!("reading macOS ACL tag for {context}"));
            }
            match tag {
                ACL_EXTENDED_DENY => continue,
                ACL_EXTENDED_ALLOW if allow_read_only => {
                    let mut permset: AclPermsetT = std::ptr::null_mut();
                    // SAFETY: `entry` is a valid extended ACL entry and `permset` is writable.
                    if unsafe { acl_get_permset(entry, &mut permset) } != 0 {
                        return Err(io::Error::last_os_error())
                            .with_context(|| format!("reading macOS ACL permset for {context}"));
                    }
                    for &perm in WRITE_PERMS {
                        // SAFETY: `permset` was populated by `acl_get_permset`.
                        if unsafe { acl_get_perm_np(permset, perm) } == 1 {
                            bail!("{context} has an extended macOS ACL granting write access");
                        }
                    }
                }
                ACL_EXTENDED_ALLOW => {
                    bail!("{context} has an extended macOS ACL granting additional access");
                }
                _ => {
                    bail!("{context} has an unrecognized extended macOS ACL");
                }
            }
        }
        Ok(())
    }
}
