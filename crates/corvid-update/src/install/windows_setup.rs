use std::cmp::Ordering;
use std::ffi::OsString;

use super::*;

/// A worker outlives the installer. Inherit no handles, including the caller's
/// redirected stdout/stderr pipes, so captured installer output reaches EOF.
pub(super) fn start_worker(executable: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW,
    };
    let application: Vec<_> = executable
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut args: Vec<_> = "corvid finish-update --retry"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    let started = unsafe {
        CreateProcessW(
            application.as_ptr(),
            args.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NO_WINDOW,
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut process,
        )
    };
    if started == 0 {
        return Err(std::io::Error::last_os_error()).context("starting deferred update cleanup");
    }
    // A successful CreateProcessW gives the caller ownership of both handles.
    let _process = unsafe { OwnedHandle::from_raw_handle(process.hProcess) };
    let _thread = unsafe { OwnedHandle::from_raw_handle(process.hThread) };
    Ok(())
}

pub(super) fn prepare(
    installation: &Installation,
    release: &Release,
    assets: &Path,
    mut progress: impl FnMut(InstallProgress),
) -> Result<()> {
    let root = installation
        .root
        .as_deref()
        .context("missing Windows Setup root")?;
    let current = current_windows_version(&WindowsInstall { root })?;
    if release.version == current {
        return Ok(());
    }
    ensure!(
        release.version.cmp_precedence(&current) == Ordering::Greater,
        "a newer version is already active"
    );
    let name = format!(
        "corvid-desktop-{}-{}-setup.exe",
        release.version, installation.package.target
    );
    let asset = release.asset(&name)?;
    let setup = assets.join("setup.exe");
    progress(InstallProgress::Downloading);
    download::file(
        &format!("{RELEASES}/download/v{}/{name}", release.version),
        &setup,
        asset.size,
    )?;
    progress(InstallProgress::Verifying);
    download::verify_file(&setup, asset.size, &asset.sha256)?;

    Ok(())
}

pub(super) fn apply(
    installation: &Installation,
    release: &Release,
    assets: &Path,
    expected: Option<&str>,
) -> Result<Installed> {
    let root = installation
        .root
        .as_deref()
        .context("locating the Windows Setup installation root")?;
    let windows = WindowsInstall { root };
    let lock = fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("setup-update.lock"))
        .context("opening the Windows Setup update lock")?;
    lock.try_lock()
        .context("another process may be updating this Windows Setup installation")?;

    let current = current_windows_version(&windows)?;
    if release.version == current {
        return installed_setup(installation, root, release);
    }
    ensure!(
        release.version.cmp_precedence(&current) == Ordering::Greater,
        "a newer version is already active"
    );

    let setup = assets.join("setup.exe");
    let mut destination = OsString::from("/DIR=");
    destination.push(root);
    ensure!(
        expected == Some(current.to_string().as_str()),
        "the active Windows Setup version changed during the download"
    );
    let status = download::command(&setup)
        .args([
            "/SP-",
            "/VERYSILENT",
            "/SUPPRESSMSGBOXES",
            "/NORESTART",
            "/NOCLOSEAPPLICATIONS",
            "/NORESTARTAPPLICATIONS",
        ])
        .arg(destination)
        .arg(format!("/CORVIDUPDATEFROM={current}"))
        .status()
        .context("running Windows Setup")?;
    ensure!(status.success(), "Windows Setup exited with {status}");
    ensure!(
        current_windows_version(&windows)? == release.version,
        "Windows Setup did not activate the selected release"
    );
    installed_setup(installation, root, release)
}

fn installed_setup(
    installation: &Installation,
    root: &Path,
    release: &Release,
) -> Result<Installed> {
    let directory = WindowsInstall { root }.version_directory(&release.version);
    let executable = directory.join("corvid.exe");
    ensure!(
        layout::for_executable(&executable)?.desktop().is_file(),
        "Windows Setup did not install the Desktop executable"
    );
    installation.installed(executable, release, String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Manifest;

    fn setup_version(root: &Path, version: &str, source_revision: &str) -> PathBuf {
        let directory = root.join("versions").join(version);
        fs::create_dir_all(&directory).unwrap();
        let package = Package {
            schema_version: 1,
            version: version.into(),
            source_revision: source_revision.into(),
            product: Product::Desktop,
            target: target(Product::Desktop).unwrap().into(),
        };
        fs::write(
            directory.join("package.json"),
            serde_json::to_vec(&package).unwrap(),
        )
        .unwrap();
        fs::write(directory.join("native-package"), "windows-setup\n").unwrap();
        fs::write(directory.join("corvid-desktop.exe"), []).unwrap();
        let executable = directory.join("corvid.exe");
        fs::write(&executable, []).unwrap();
        executable
    }

    #[test]
    fn reuses_the_active_setup_release_and_checks_its_identity() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let original = setup_version(root, "0.1.0", &"a".repeat(40));
        let updated = setup_version(root, "0.2.0", &"b".repeat(40));
        fs::write(root.join("current.txt"), "0.2.0\r\n").unwrap();
        let installation = Installation::at(&original).unwrap();
        assert!(installation.is_windows_setup());
        assert_eq!(installation.root.as_deref(), Some(root));

        let mut release = Release {
            version: version("0.2.0").unwrap(),
            notes: String::new(),
            manifest: Manifest {
                schema_version: 1,
                version: "0.2.0".into(),
                source_revision: "b".repeat(40),
                assets: Vec::new(),
            },
        };
        assert_eq!(installation.install(&release).unwrap().executable, updated);
        release.manifest.source_revision = "c".repeat(40);
        let result = installation.install(&release);
        assert!(matches!(result, Err(error) if error.to_string() == "installed package mismatch"));
        release.version = version("0.1.0").unwrap();
        let result = installation.install(&release);
        assert!(matches!(result, Err(error) if error.to_string().contains("newer version")));
    }
}
