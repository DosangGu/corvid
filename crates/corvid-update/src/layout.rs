//! Filesystem paths used by Corvid packages, launchers, and source builds.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use semver::Version;

#[cfg(test)]
mod tests;

/// Paths derived from the directory structure around an executable.
/// The caller checks package metadata and the files it intends to use.
pub trait PackageLayout {
    fn binary_directory(&self) -> &Path;
    fn package_directory(&self) -> &Path;
    fn resource_directory(&self) -> PathBuf;
    fn executable_suffix(&self) -> &'static str;

    fn app_helper(&self) -> Option<PathBuf> {
        None
    }

    fn package_metadata(&self) -> PathBuf {
        self.resource_directory().join("package.json")
    }

    fn local_daemon(&self) -> PathBuf {
        self.binary_directory()
            .join(format!("corvid{}", self.executable_suffix()))
    }

    fn desktop(&self) -> PathBuf {
        self.binary_directory()
            .join(format!("corvid-desktop{}", self.executable_suffix()))
    }

    fn remote_runtime(&self, target: &str) -> PathBuf {
        self.resource_directory().join("daemon").join(target)
    }

    fn agent_deck_candidates(&self) -> Vec<PathBuf> {
        let directory = self.binary_directory();
        let binary = format!("corvid-agent-deck{}", self.executable_suffix());
        self.app_helper()
            .into_iter()
            .chain([
                directory.join("plugins/agent-deck").join(&binary),
                directory.join(binary),
            ])
            .collect()
    }
}

struct FlatPackage<'a> {
    directory: &'a Path,
    suffix: &'static str,
    macos: bool,
}

impl PackageLayout for FlatPackage<'_> {
    fn binary_directory(&self) -> &Path {
        self.directory
    }

    fn package_directory(&self) -> &Path {
        self.directory
    }

    fn resource_directory(&self) -> PathBuf {
        self.directory.to_owned()
    }

    fn executable_suffix(&self) -> &'static str {
        self.suffix
    }

    fn app_helper(&self) -> Option<PathBuf> {
        self.macos.then(|| macos_helper(&self.directory.join("..")))
    }
}

struct MacAppBundle<'a> {
    binaries: &'a Path,
    contents: &'a Path,
    app: &'a Path,
}

impl PackageLayout for MacAppBundle<'_> {
    fn binary_directory(&self) -> &Path {
        self.binaries
    }

    fn package_directory(&self) -> &Path {
        // The installer puts the .app alongside the package's version metadata.
        self.app.parent().unwrap_or(self.app)
    }

    fn resource_directory(&self) -> PathBuf {
        self.contents.join("Resources")
    }

    fn executable_suffix(&self) -> &'static str {
        ""
    }

    fn app_helper(&self) -> Option<PathBuf> {
        Some(macos_helper(self.contents))
    }
}

fn macos_helper(contents: &Path) -> PathBuf {
    contents.join("Helpers/Corvid Agent Deck.app/Contents/MacOS/corvid-agent-deck")
}

pub fn for_executable(executable: &Path) -> Result<Box<dyn PackageLayout + '_>> {
    for_platform(executable, std::env::consts::OS)
}

/// A packaged Desktop delegates daemon work to its adjacent runtime.
/// Standalone and development executables can run the daemon themselves.
pub fn local_daemon_executable(executable: &Path) -> Result<PathBuf> {
    local_daemon_for_platform(executable, std::env::consts::OS)
}

fn local_daemon_for_platform(executable: &Path, os: &str) -> Result<PathBuf> {
    let layout = for_platform(executable, os)?;
    if executable.file_stem() != Some("corvid-desktop".as_ref())
        || !layout.package_metadata().is_file()
    {
        return Ok(executable.to_owned());
    }
    let runtime = layout.local_daemon();
    ensure!(runtime.is_file(), "the packaged Corvid runtime is missing");
    Ok(runtime)
}

pub fn agent_deck_executable(executable: &Path) -> Result<PathBuf> {
    agent_deck_for_platform(executable, std::env::consts::OS)
}

fn agent_deck_for_platform(executable: &Path, os: &str) -> Result<PathBuf> {
    for_platform(executable, os)?
        .agent_deck_candidates()
        .into_iter()
        .find(|candidate| candidate.is_file())
        .context("The Agents window is missing from this installation. Reinstall Corvid Desktop to restore it.")
}

fn for_platform<'a>(executable: &'a Path, os: &str) -> Result<Box<dyn PackageLayout + 'a>> {
    let directory = executable
        .parent()
        .context("locating Corvid installation")?;
    if os == "macos" && directory.ends_with("Contents/MacOS") {
        let contents = directory.parent().context("locating app contents")?;
        let app = contents.parent().context("locating macOS app bundle")?;
        // A relative Contents/MacOS path can be used by an uninstalled build.
        // Its bundle still has Resources, with no containing package directory.
        return Ok(Box::new(MacAppBundle {
            binaries: directory,
            contents,
            app,
        }));
    }

    let suffix = if os == "windows" { ".exe" } else { "" };
    Ok(Box::new(FlatPackage {
        directory,
        suffix,
        macos: os == "macos",
    }))
}

pub(crate) struct VersionedPackage<'a> {
    pub root: &'a Path,
    pub directory: &'a Path,
}

impl<'a> VersionedPackage<'a> {
    pub fn at(directory: &'a Path) -> Option<Self> {
        let versions = directory.parent()?;
        if versions.file_name()? != "versions" {
            return None;
        }
        Some(Self {
            root: versions.parent()?,
            directory,
        })
    }

    pub fn receipt(&self) -> PathBuf {
        self.root.join("install.json")
    }

    fn active_path(&self, executable: &Path) -> Option<PathBuf> {
        let relative = executable.strip_prefix(self.directory).ok()?;
        Some(self.root.join("current").join(relative))
    }
}

/// Convert a versioned executable in a managed Unix installation to the path
/// through its current symlink, including a nested helper's bundle components.
pub fn unix_startup_executable(executable: &Path) -> PathBuf {
    executable
        .ancestors()
        .skip(1)
        .filter_map(VersionedPackage::at)
        .find(|package| package.receipt().is_file())
        .and_then(|package| package.active_path(executable))
        .unwrap_or_else(|| executable.to_owned())
}

pub(crate) struct WindowsInstall<'a> {
    pub root: &'a Path,
}

impl<'a> WindowsInstall<'a> {
    pub fn from_launcher(executable: &'a Path) -> Result<Self> {
        let root = executable
            .parent()
            .and_then(Path::parent)
            .context("locating installation")?;
        Ok(Self { root })
    }

    pub fn marker(directory: &Path) -> PathBuf {
        directory.join("native-package")
    }

    pub fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }

    pub fn pointer(&self) -> PathBuf {
        self.root.join("current.txt")
    }

    pub fn version_directory(&self, version: &Version) -> PathBuf {
        self.root.join("versions").join(version.to_string())
    }
}

/// Locate the Windows Setup launcher for a packaged Agent Deck shortcut.
pub fn windows_setup_deck_launcher(executable: &Path) -> Option<PathBuf> {
    let binary_dir = executable.parent()?;
    if !binary_dir.ends_with("plugins/agent-deck") {
        return None;
    }
    let directory = binary_dir.ancestors().nth(2)?;
    let package = VersionedPackage::at(directory)?;
    let marker = fs::read_to_string(WindowsInstall::marker(directory)).ok()?;
    if marker.trim() != "windows-setup" {
        return None;
    }
    let launcher = WindowsInstall { root: package.root }
        .bin()
        .join("corvid-agent-deck.exe");
    launcher.is_file().then_some(launcher)
}
