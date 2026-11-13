use std::cmp::Ordering;
use std::ffi::OsString;

use super::*;

pub(super) fn install(
    installation: &Installation,
    release: &Release,
    mut progress: impl FnMut(InstallProgress),
) -> Result<Installed> {
    let root = installation
        .root
        .as_deref()
        .context("locating the Windows Setup installation root")?;
    let lock = fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("setup-update.lock"))
        .context("opening the Windows Setup update lock")?;
    lock.try_lock()
        .context("another process may be updating this Windows Setup installation")?;

    let current = current_windows_version(root)?;
    if release.version == current {
        return installed_setup(installation, root, release);
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
    let temporary = tempfile::tempdir()?;
    let setup = temporary.path().join(&name);
    progress(InstallProgress::Downloading);
    download::file(
        &format!("{RELEASES}/download/v{}/{name}", release.version),
        &setup,
        asset.size,
    )?;
    progress(InstallProgress::Verifying);
    download::verify_file(&setup, asset.size, &asset.sha256)?;

    let mut destination = OsString::from("/DIR=");
    destination.push(root);
    ensure!(
        current_windows_version(root)? == current,
        "the active Windows Setup version changed during the download"
    );
    progress(InstallProgress::Installing);
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
        current_windows_version(root)? == release.version,
        "Windows Setup did not activate the selected release"
    );
    installed_setup(installation, root, release)
}

fn installed_setup(
    installation: &Installation,
    root: &Path,
    release: &Release,
) -> Result<Installed> {
    let directory = root.join("versions").join(release.version.to_string());
    ensure!(
        directory.join("corvid-desktop.exe").is_file(),
        "Windows Setup did not install the Desktop executable"
    );
    installation.installed(directory.join("corvid.exe"), release, String::new())
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
