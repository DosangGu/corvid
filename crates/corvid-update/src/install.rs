use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::{Package, Product, RELEASES, Release, download, target, version};

#[derive(Clone, Debug)]
pub struct Installation {
    pub package: Package,
    pub root: Option<PathBuf>,
    pub bin_dir: Option<PathBuf>,
    native_package: bool,
}

#[derive(Deserialize)]
struct Receipt {
    schema_version: u32,
    product: Product,
}

pub struct Installed {
    pub executable: PathBuf,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallProgress {
    Downloading,
    Verifying,
    Installing,
}

impl Installation {
    pub fn current() -> Result<Self> {
        Self::detect()?.context(
            "This is a source build or an externally managed installation. Update it using its original installation method.",
        )
    }

    pub fn detect() -> Result<Option<Self>> {
        let executable = env::current_exe()?;
        let directory = executable.parent().context("locating the installation")?;
        if !directory.join("package.json").try_exists()? {
            return Ok(None);
        }
        Self::at(&executable).map(Some)
    }

    pub fn at(executable: &Path) -> Result<Self> {
        let directory = executable.parent().context("locating the installation")?;
        let metadata = directory.join("package.json");
        ensure!(
            metadata.is_file(),
            "This is a source build or an externally managed installation. Update it using its original installation method."
        );
        let package: Package = serde_json::from_slice(&fs::read(metadata)?)?;
        ensure!(package.schema_version == 1, "unsupported package metadata");
        version(&package.version)?;
        ensure!(
            package.target == target(package.product)?,
            "package platform mismatch"
        );
        let root = directory
            .parent()
            .filter(|parent| parent.file_name().is_some_and(|name| name == "versions"))
            .and_then(Path::parent)
            .filter(|root| root.join("install.json").is_file());
        let bin_dir = if let Some(root) = root {
            let receipt: Receipt = serde_json::from_slice(&fs::read(root.join("install.json"))?)?;
            ensure!(
                receipt.schema_version == 1 && receipt.product == package.product,
                "installation receipt mismatch"
            );
            let bin = fs::read_to_string(root.join("bin-dir"))?;
            let bin = PathBuf::from(bin.trim_end_matches(['\r', '\n']));
            ensure!(
                bin.is_absolute(),
                "installation bin directory must be absolute"
            );
            Some(bin)
        } else {
            None
        };
        Ok(Self {
            package,
            root: root.map(Path::to_owned),
            bin_dir,
            native_package: directory.join("native-package").is_file(),
        })
    }

    pub fn supports_in_app_update(&self) -> bool {
        !self.native_package
    }

    /// Runs a verified, version-pinned installer. Never contacts or restarts a daemon.
    pub fn install(&self, release: &Release) -> Result<Installed> {
        self.install_with_progress(release, |_| {})
    }

    pub fn install_with_progress(
        &self,
        release: &Release,
        mut progress: impl FnMut(InstallProgress),
    ) -> Result<Installed> {
        ensure!(
            self.supports_in_app_update(),
            "Update this installation using its original installer."
        );
        let product = self.package.product;
        let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
        let archive = format!(
            "corvid-{}-{}-{}.{}",
            product.as_str(),
            release.version,
            target(product)?,
            extension
        );
        let asset = release.asset(&archive)?;
        let script_name = if cfg!(windows) {
            "install.ps1"
        } else {
            "install.sh"
        };
        let script_asset = release.asset(script_name)?;
        ensure!(
            script_asset.size <= 256 * 1024,
            "installer exceeds size limit"
        );
        progress(InstallProgress::Downloading);
        let script = download::bytes(
            &format!("{RELEASES}/download/v{}/{script_name}", release.version),
            256 * 1024,
        )?;
        download::verify(&script, script_asset.size, &script_asset.sha256)?;
        let temporary = tempfile::tempdir()?;
        let script_path = temporary.path().join(script_name);
        fs::write(&script_path, script)?;
        let archive_path = temporary.path().join(&archive);
        download::file(
            &format!("{RELEASES}/download/v{}/{archive}", release.version),
            &archive_path,
            asset.size,
        )?;
        progress(InstallProgress::Verifying);
        download::verify_file(&archive_path, asset.size, &asset.sha256)?;
        fs::write(
            temporary.path().join(format!("{archive}.sha256")),
            format!("{}  {archive}\n", asset.sha256),
        )?;
        let mut command = self.installer_command(&script_path, temporary.path(), release, asset);
        progress(InstallProgress::Installing);
        let output = command.output().context("running the Corvid installer")?;
        ensure!(
            output.status.success(),
            "installation failed: {}\n{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        let message = String::from_utf8(output.stdout).context("reading installer result")?;
        let installed = message
            .lines()
            .find_map(|line| line.strip_prefix("CORVID_EXECUTABLE="))
            .context("installer did not return its executable")?;
        let executable = PathBuf::from(installed);
        ensure!(
            executable.is_absolute() && executable.is_file(),
            "invalid installed executable"
        );
        let installed_package = Self::at(&executable)?;
        ensure!(
            installed_package.package.version == release.version.to_string()
                && installed_package.package.product == product
                && installed_package.package.source_revision == release.manifest.source_revision,
            "installed package mismatch"
        );
        Ok(Installed {
            executable,
            message,
        })
    }

    fn installer_command(
        &self,
        script_path: &Path,
        source: &Path,
        release: &Release,
        asset: &crate::Asset,
    ) -> Command {
        let product = self.package.product;
        if cfg!(windows) {
            let mut command = download::command("powershell.exe");
            command.env_remove("PSModulePath");
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ]);
            command
                .arg(script_path)
                .arg("-SourceDirectory")
                .arg(source)
                .arg("-Version")
                .arg(release.version.to_string())
                .arg("-Product")
                .arg(product.as_str())
                .arg("-ArchiveSha256")
                .arg(&asset.sha256);
            if let Some(root) = &self.root {
                command.arg("-InstallRoot").arg(root);
            }
            command
        } else {
            let mut command = download::command("sh");
            command
                .arg(script_path)
                .arg("--source-dir")
                .arg(source)
                .arg("--version")
                .arg(release.version.to_string())
                .arg("--product")
                .arg(product.as_str())
                .arg("--archive-sha256")
                .arg(&asset.sha256);
            if let Some(root) = &self.root {
                command.arg("--install-root").arg(root);
            }
            if let Some(bin) = &self.bin_dir {
                command.arg("--bin-dir").arg(bin);
            }
            command
        }
    }
}

impl Installed {
    pub fn start_desktop(&self) -> Result<()> {
        let directory = self.executable.parent().context("locating Desktop")?;
        let binary = directory.join(if cfg!(windows) {
            "corvid-desktop.exe"
        } else {
            "corvid-desktop"
        });
        let mut command = download::command(binary.to_str().context("invalid Desktop path")?);
        command.args(env::args_os().skip(1));
        command.spawn().context("restarting Corvid Desktop")?;
        Ok(())
    }
}

/// Stable Windows entry point: only the current-version pointer changes on update.
pub fn launch(binary: &str, wait: bool) -> Result<ExitCode> {
    let executable = env::current_exe()?;
    let root = executable
        .parent()
        .and_then(Path::parent)
        .context("locating installation")?;
    let current = fs::read_to_string(root.join("current.txt"))?;
    let selected = version(current.trim())?;
    ensure!(
        current.trim() == selected.to_string(),
        "invalid current version"
    );
    let mut command = Command::new(
        root.join("versions")
            .join(selected.to_string())
            .join(binary),
    );
    command.args(env::args_os().skip(1));
    if !wait {
        command.spawn().context("starting Corvid application")?;
        return Ok(ExitCode::SUCCESS);
    }
    let status = command.status().context("starting Corvid")?;
    Ok(ExitCode::from(
        status
            .code()
            .and_then(|code| code.try_into().ok())
            .unwrap_or(1),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_builds_cannot_be_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        assert!(Installation::at(&directory.path().join("corvid")).is_err());
    }

    #[test]
    fn desktop_cli_keeps_desktop_product() {
        let directory = tempfile::tempdir().unwrap();
        let package = Package {
            schema_version: 1,
            version: "0.1.0".into(),
            source_revision: "a".repeat(40),
            product: Product::Desktop,
            target: target(Product::Desktop).unwrap().into(),
        };
        fs::write(
            directory.path().join("package.json"),
            serde_json::to_vec(&package).unwrap(),
        )
        .unwrap();
        let installation = Installation::at(&directory.path().join("corvid")).unwrap();
        assert_eq!(installation.package.product, Product::Desktop);
        assert!(installation.root.is_none());
        assert!(installation.supports_in_app_update());

        fs::write(directory.path().join("native-package"), "native\n").unwrap();
        let installation = Installation::at(&directory.path().join("corvid")).unwrap();
        assert!(!installation.supports_in_app_update());
    }
}
