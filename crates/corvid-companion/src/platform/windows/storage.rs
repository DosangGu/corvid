use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr::{null, null_mut};

use anyhow::{Context, Result, bail, ensure};
use uuid::Uuid;
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateDirectoryW, CreateFileW,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_TYPE_DISK, GetDriveTypeW, GetFileInformationByHandle, GetFileType, GetVolumeInformationW,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_ALWAYS, OPEN_EXISTING,
    READ_CONTROL,
};
use windows_sys::Win32::System::SystemServices::FILE_PERSISTENT_ACLS;
use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;

use super::security::{PrivateDescriptor, checked, current_user, verify_private};

pub(crate) struct StateDirectory {
    path: PathBuf,
    user: String,
    _ancestors: Vec<File>,
}

impl StateDirectory {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        Self::open_path(path, false)
    }

    pub(crate) fn create_all(path: &Path) -> Result<Self> {
        Self::open_path(path, true)
    }

    fn open_path(path: &Path, create: bool) -> Result<Self> {
        let mut components = path.components().peekable();
        let Some(Component::Prefix(prefix)) = components.next() else {
            bail!("Windows companion state requires an absolute local drive path");
        };
        let drive = match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => bail!("Windows companion state requires an absolute local drive path"),
        };
        ensure!(
            matches!(components.next(), Some(Component::RootDir)) && components.peek().is_some(),
            "Windows companion state requires a directory below a local drive root"
        );
        require_local_acl_volume(drive)?;

        let user = current_user()?;
        let descriptor = PrivateDescriptor::new(&user, true)?;
        let mut path = PathBuf::from(format!(r"\\?\{}:\", char::from(drive)));
        let mut ancestors = Vec::new();
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                bail!("Windows state path contains traversal")
            };
            check_component(name)?;
            path.push(name);
            let last = components.peek().is_none();
            let access = FILE_READ_ATTRIBUTES | if last { READ_CONTROL } else { 0 };
            let directory = match open_directory(&path, access) {
                Err(error) if create && error.kind() == ErrorKind::NotFound => {
                    create_directory(&path, &descriptor)?;
                    open_directory(&path, access)?
                }
                result => result?,
            };
            check_type(&directory, true)?;
            if last {
                verify_private(directory.as_raw_handle(), &user, true)?;
            }
            // Retaining handles without delete sharing prevents ancestor substitution.
            ancestors.push(directory);
        }
        Ok(Self {
            path,
            user,
            _ancestors: ancestors,
        })
    }

    pub(crate) fn lock_storage(&self) -> Result<File> {
        let path = self.file_path("state.lock")?;
        let descriptor = PrivateDescriptor::new(&self.user, false)?;
        let lock = open_file(
            &path,
            GENERIC_READ | GENERIC_WRITE,
            0,
            OPEN_ALWAYS,
            Some(&descriptor.attributes()),
        )
        .context("acquiring private Windows state lock")?;
        self.check_file(&lock)?;
        Ok(lock)
    }

    pub(crate) fn read_private_file(&self, name: &str, max_bytes: u64) -> Result<Option<Vec<u8>>> {
        let path = self.file_path(name)?;
        let Some(file) = self.open_existing(&path, GENERIC_READ)? else {
            return Ok(None);
        };
        ensure!(
            file.metadata()?.len() <= max_bytes,
            "{name} exceeds its size limit"
        );
        let mut bytes = Vec::new();
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= max_bytes,
            "{name} exceeds its size limit"
        );
        Ok(Some(bytes))
    }

    pub(crate) fn replace_private_file(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let destination = self.file_path(name)?;
        drop(self.open_existing(&destination, FILE_READ_ATTRIBUTES | READ_CONTROL)?);
        let temporary = self.path.join(format!(".corvid-{}.tmp", Uuid::new_v4()));
        let descriptor = PrivateDescriptor::new(&self.user, false)?;
        let mut file = open_file(
            &temporary,
            GENERIC_READ | GENERIC_WRITE,
            0,
            CREATE_NEW,
            Some(&descriptor.attributes()),
        )?;
        let result = (|| {
            self.check_file(&file)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            let source = wide(&temporary);
            let destination = wide(&destination);
            // Both paths are NUL-terminated and stay in the locked private directory.
            checked(unsafe {
                MoveFileExW(
                    source.as_ptr(),
                    destination.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            })
            .with_context(|| format!("publishing {name}"))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn open_existing(&self, path: &Path, access: u32) -> Result<Option<File>> {
        let file = match open_file(path, access, FILE_SHARE_READ, OPEN_EXISTING, None) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("opening private Windows state file"),
        };
        self.check_file(&file)?;
        Ok(Some(file))
    }

    fn check_file(&self, file: &File) -> Result<()> {
        check_type(file, false)?;
        verify_private(file.as_raw_handle(), &self.user, true)
    }

    fn file_path(&self, name: &str) -> Result<PathBuf> {
        let mut components = Path::new(name).components();
        let Some(Component::Normal(component)) = components.next() else {
            bail!("invalid private file name")
        };
        ensure!(
            components.next().is_none() && component == name,
            "invalid private file name"
        );
        check_component(component)?;
        Ok(self.path.join(component))
    }
}

fn require_local_acl_volume(drive: u8) -> Result<()> {
    let root: Vec<_> = format!(r"{}:\", char::from(drive))
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // The root is a NUL-terminated drive path.
    ensure!(
        unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_FIXED,
        "Windows state requires a local fixed drive"
    );
    let mut flags = 0;
    // The API writes the volume flags and all optional buffers are null.
    checked(unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            &mut flags,
            null_mut(),
            0,
        )
    })?;
    ensure!(
        flags & FILE_PERSISTENT_ACLS != 0,
        "Windows state requires persistent file ACLs"
    );
    Ok(())
}

fn check_component(name: &OsStr) -> Result<()> {
    let mut last = None;
    for unit in name.encode_wide() {
        ensure!(
            unit >= 32 && ![b':' as u16, b'/' as u16, b'\\' as u16].contains(&unit),
            "invalid Windows state path component"
        );
        last = Some(unit);
    }
    ensure!(
        !matches!(last, Some(32 | 46) | None),
        "invalid trailing character in Windows state path"
    );
    Ok(())
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn create_directory(path: &Path, descriptor: &PrivateDescriptor) -> Result<()> {
    let path = wide(path);
    let attributes = descriptor.attributes();
    // The path and security attributes are valid for the duration of the call.
    match checked(unsafe { CreateDirectoryW(path.as_ptr(), &attributes) }) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).context("creating private Windows state directory"),
    }
}

fn open_directory(path: &Path, access: u32) -> io::Result<File> {
    open_file(
        path,
        access,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        OPEN_EXISTING,
        None,
    )
}

fn open_file(
    path: &Path,
    access: u32,
    sharing: u32,
    disposition: u32,
    attributes: Option<&SECURITY_ATTRIBUTES>,
) -> io::Result<File> {
    let path = wide(path);
    // The path is NUL-terminated. The optional descriptor outlives CreateFileW.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            sharing,
            attributes.map_or(null(), std::ptr::from_ref),
            disposition,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // CreateFileW returned a new owned handle.
    Ok(unsafe { File::from_raw_handle(handle) })
}

fn check_type(file: &File, directory: bool) -> Result<()> {
    let handle = file.as_raw_handle();
    // The handle is live and belongs to this File.
    ensure!(
        unsafe { GetFileType(handle) } == FILE_TYPE_DISK,
        "Windows state path is not a disk file"
    );
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // Windows fills the writable information structure for the live handle.
    checked(unsafe { GetFileInformationByHandle(handle, &mut info) })?;
    ensure!(
        info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "Windows state path contains a reparse point"
    );
    ensure!(
        (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) == directory,
        "Windows state path has the wrong file type"
    );
    ensure!(
        directory || info.nNumberOfLinks == 1,
        "Windows state file has hard links"
    );
    Ok(())
}
