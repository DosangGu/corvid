//! Update a Finder-installed bundle without changing its outer directory identity.
use std::env;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use crate::{Installation, Product};

const BUNDLE: &str = "Corvid.app";
const APP_LINK: &str = "macos-app";
const DESKTOP: &str = "Contents/MacOS/corvid-desktop";
const CLI: &str = "Contents/MacOS/corvid";
const METADATA: &str = "Contents/Resources/package.json";
const IDENTIFIER: &str = "=identifier \"org.corvid.desktop\"";

// Invoked by the verified installer while its initiating Desktop or CLI is alive.
// Exit 3 lets the installer continue a standalone script installation.
pub fn replace_from_installer(caller: i32) -> Result<u32> {
    let Some(app) = calling_bundle(caller)? else {
        ensure!(
            env::var_os("CORVID_EXPECT_APP_REVISION").is_none(),
            "the installed Corvid app could not be identified"
        );
        return Ok(3);
    };
    let path = CString::new(app.as_os_str().as_bytes())?;
    let mut volume = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // statfs initializes the output on success.
    if unsafe { libc::statfs(path.as_ptr(), volume.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error()).context("inspecting Corvid's installation volume");
    }
    let volume = unsafe { volume.assume_init() };
    ensure!(
        volume.f_flags & libc::MNT_RDONLY as u32 == 0,
        "Copy Corvid.app from the disk image to Applications and reopen it before updating."
    );
    let binary = app.join(CLI);
    let installed = Installation::at(&binary)?;
    ensure!(
        installed.package.product == Product::Desktop,
        "the app is not a Corvid Desktop package"
    );
    if let Some(expected) = env::var_os("CORVID_EXPECT_APP_REVISION") {
        ensure!(
            expected == installed.package.source_revision.as_str(),
            "Corvid changed while the update was being prepared"
        );
    }
    let incoming_cli = env::current_exe()?.canonicalize()?;
    if let Some(root) = installed.root {
        // Older updaters launched through the legacy app symlink can omit
        // --install-root. Continue only if the installer staged in that root.
        ensure!(
            incoming_cli.ancestors().nth(6) == Some(root.as_path()),
            "the app belongs to another script installation; use its original installer"
        );
        return Ok(3);
    }
    ensure!(
        !app.as_os_str().as_bytes().contains(&b'\n'),
        "Corvid cannot update an app path containing a newline"
    );

    let incoming = incoming_cli
        .ancestors()
        .nth(3)
        .context("locating staged bundle")?;
    ensure!(
        incoming.join(CLI) == incoming_cli && incoming != app,
        "invalid staged bundle"
    );
    verify(&app)?;
    verify(incoming)?;
    let directory = fs::File::open(&app)?;
    // Lock the persistent outer directory so simultaneous updates cannot exchange Contents.
    if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error()).context("locking the installed Corvid app");
    }
    let expected = digest(&app)?;
    match replace(&app, incoming, &expected) {
        Ok(()) => {}
        Err(error) if os_error(&error) == Some(libc::EROFS) => {
            bail!(
                "Copy Corvid.app from the disk image to Applications and reopen it before updating."
            );
        }
        Err(error) if matches!(os_error(&error), Some(libc::EACCES | libc::EPERM)) => {
            authorize(&app, incoming, &expected)?;
        }
        Err(error) => return Err(error),
    }
    println!(
        "Installed Corvid {} in {}.",
        env!("CARGO_PKG_VERSION"),
        app.display()
    );
    println!("CORVID_EXECUTABLE={}", binary.display());
    Ok(0)
}

/// Publish the GUI bundle while the script holds the managed installation lock.
pub fn publish_managed(root: &Path, app: &Path) -> Result<u32> {
    let executable = env::current_exe()?;
    let incoming = executable
        .ancestors()
        .nth(3)
        .context("locating the incoming app")?;
    ensure!(
        incoming.join(CLI) == executable
            && incoming.parent().and_then(Path::parent) == Some(root.join("versions").as_path()),
        "expected a managed Corvid Desktop package"
    );
    ensure!(
        app.is_absolute() && app.file_name() == Some(OsStr::new(BUNDLE)),
        "invalid managed Corvid app path"
    );
    verify(incoming)?;
    let link = root.join(APP_LINK);
    if link.symlink_metadata().is_ok() {
        ensure!(
            fs::read_link(&link)? == app,
            "Corvid's registered app path changed"
        );
    } else {
        symlink(app, &link)?;
    }

    let installed = match app.symlink_metadata() {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if installed.as_ref().is_some_and(|file| !file.is_symlink()) {
        verify(app)?;
        if digest(app)? == digest(incoming)? {
            return report_managed_app(app);
        }
    }

    let parent = app.parent().context("locating the app directory")?;
    let staging = tempfile::Builder::new()
        .prefix(".corvid-update-")
        .tempdir_in(parent)?;
    let next = staging.path().join(BUNDLE);
    copy_bundle(incoming, &next)?;
    verify(&next)?;

    match installed {
        None => rename(&next, app, libc::RENAME_EXCL).context("installing the Corvid app")?,
        Some(file) if file.is_symlink() => migrate_bundle(root, app, &next)?,
        Some(_) => exchange_contents(app, &next)?,
    }
    report_managed_app(app)
}

fn report_managed_app(app: &Path) -> Result<u32> {
    println!("CORVID_EXECUTABLE={}", app.join(CLI).display());
    Ok(0)
}

fn migrate_bundle(root: &Path, app: &Path, next: &Path) -> Result<()> {
    let current = root.join("current").join(BUNDLE);
    ensure!(
        fs::read_link(app)? == current,
        "the existing app link belongs to another installation"
    );
    let previous = fs::canonicalize(current)?;
    ensure!(
        previous.parent().and_then(Path::parent) == Some(root.join("versions").as_path()),
        "invalid previous app directory"
    );
    verify(&previous)?;

    // Keep the outer directory recognized by the Dock. Put the old Contents back
    // under its version directory so retirement can still identify running images.
    exchange_contents(&previous, next)?;
    if let Err(error) = rename(&previous, app, libc::RENAME_SWAP) {
        exchange_contents(&previous, next).context("restoring the previous app contents")?;
        return Err(error).context("moving the bookmarked app to its permanent location");
    }
    if let Err(error) = rename(next, &previous, libc::RENAME_SWAP) {
        rename(app, &previous, libc::RENAME_SWAP).context("restoring the previous app link")?;
        exchange_contents(&previous, next).context("restoring the previous app contents")?;
        return Err(error).context("returning the previous executables to their package");
    }
    Ok(())
}

/// Resolve the installer receipt for a stable app. A bundle installed from a
/// disk image has no matching registration.
pub(crate) fn managed_root(app: &Path) -> Option<PathBuf> {
    let parent = app.parent()?;
    let default_root = env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support/Corvid/install/desktop"));
    std::iter::once(parent.to_owned())
        .chain(default_root)
        .find_map(|root| {
            let root = fs::canonicalize(root).ok()?;
            (root.join("install.json").is_file() && fs::read_link(root.join(APP_LINK)).ok()? == app)
                .then_some(root)
        })
}

/// Launch persistent services from the version store so the package retirement
/// worker can identify them even after the public bundle's contents change.
pub(crate) fn versioned_runtime(executable: &Path) -> Option<PathBuf> {
    let app = executable.ancestors().nth(3)?;
    let root = managed_root(app)?;
    let install = Installation::at(executable).ok()?;
    let path = root
        .join("versions")
        .join(install.package.version)
        .join(BUNDLE)
        .join(CLI);
    path.is_file().then_some(path)
}

/// Older Desktop builds and saved launch paths may still open a versioned GUI.
pub fn use_registered_desktop() -> Result<()> {
    let executable = env::current_exe()?.canonicalize()?;
    let Some(versions) = executable
        .ancestors()
        .nth(5)
        .filter(|dir| dir.file_name() == Some(OsStr::new("versions")))
    else {
        return Ok(());
    };
    let Some(root) = versions.parent() else {
        return Ok(());
    };
    let Ok(app) = fs::read_link(root.join(APP_LINK)) else {
        return Ok(());
    };
    let Some(versioned) = executable.ancestors().nth(3) else {
        return Ok(());
    };
    let same_release = fs::read(app.join(METADATA))
        .ok()
        .zip(fs::read(versioned.join(METADATA)).ok())
        .is_some_and(|(public, package)| public == package);
    if managed_root(&app).as_deref() != Some(root) || !same_release {
        return Ok(());
    }
    let error = Command::new(app.join(DESKTOP))
        .args(env::args_os().skip(1))
        .exec();
    Err(error).context("opening the registered Corvid app")
}

fn calling_bundle(caller: i32) -> Result<Option<PathBuf>> {
    if caller <= 1 {
        return Ok(None);
    }
    let mut path = [0_u8; 4096];
    // libproc writes at most the initialized buffer size and terminates a successful path.
    let size = unsafe { libc::proc_pidpath(caller, path.as_mut_ptr().cast(), path.len() as u32) };
    if size <= 0 {
        return Ok(None);
    }
    let executable = PathBuf::from(OsString::from_vec(
        CStr::from_bytes_until_nul(&path)?.to_bytes().to_vec(),
    ));
    if !matches!(
        executable.file_name().and_then(OsStr::to_str),
        Some("corvid" | "corvid-desktop")
    ) || !executable
        .parent()
        .is_some_and(|dir| dir.ends_with("Contents/MacOS"))
    {
        return Ok(None);
    }
    let app = executable
        .ancestors()
        .nth(3)
        .context("locating the installed app")?;
    ensure!(
        app.extension() == Some(OsStr::new("app")),
        "invalid Corvid app path"
    );
    Ok(Some(fs::canonicalize(app)?))
}

fn verify(app: &Path) -> Result<()> {
    let result = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict", "-R", IDENTIFIER])
        .arg(app)
        .output()?;
    ensure!(
        result.status.success(),
        "Corvid app signature check failed: {}",
        String::from_utf8_lossy(&result.stderr).trim()
    );
    Ok(())
}

fn digest(app: &Path) -> Result<String> {
    Ok(hex::encode(Sha256::digest(fs::read(app.join(METADATA))?)))
}

fn os_error(error: &anyhow::Error) -> Option<i32> {
    error.downcast_ref::<io::Error>()?.raw_os_error()
}

fn replace(app: &Path, incoming: &Path, expected: &str) -> Result<()> {
    let staging = tempfile::Builder::new()
        .prefix(".corvid-update-")
        .tempdir_in(app)?;
    let staged_app = staging.path().join("Corvid.app");
    copy_bundle(incoming, &staged_app)?;
    verify(&staged_app)?;
    ensure!(
        digest(app)? == expected,
        "Corvid changed while the update was being prepared"
    );

    exchange_contents(app, &staged_app)?;
    if let Err(error) = staging.close() {
        eprintln!("Installed Corvid. Could not remove the previous app contents: {error}");
    }
    Ok(())
}

fn copy_bundle(incoming: &Path, destination: &Path) -> Result<()> {
    // Preserve the sealed bundle. APFS can share its data with the versioned
    // package until either copy changes.
    let copy = Command::new("/bin/cp")
        .args(["-cRp"])
        .arg(incoming)
        .arg(destination)
        .output()?;
    ensure!(
        copy.status.success(),
        "copying the verified app: {}",
        String::from_utf8_lossy(&copy.stderr).trim()
    );
    Ok(())
}

fn exchange_contents(app: &Path, next: &Path) -> Result<()> {
    // Keep the outer directory's file identity for Finder and Dock bookmarks.
    rename(
        &app.join("Contents"),
        &next.join("Contents"),
        libc::RENAME_SWAP,
    )
    .context("exchanging Corvid app contents")
}

fn rename(from: &Path, to: &Path, flag: u32) -> io::Result<()> {
    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    // The paths are NUL terminated and live through the call.
    if unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            flag,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn authorize(app: &Path, incoming: &Path, expected: &str) -> Result<()> {
    // Only Apple tools run as root. The shell program is fixed and osascript quotes
    // each path as one argument. The script checks destination ownership as root.
    let signature = Command::new("/usr/bin/codesign")
        .args(["--display", "--verbose=4"])
        .arg(incoming)
        .output()?;
    ensure!(
        signature.status.success(),
        "could not read the staged app signature"
    );
    let report = String::from_utf8(signature.stderr)?;
    let cdhash = report
        .lines()
        .find_map(|line| line.strip_prefix("CDHash="))
        .context("missing code directory hash for the staged app")?;
    ensure!(
        cdhash.len() == 40 && cdhash.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid staged code directory hash"
    );

    let result = Command::new("/usr/bin/osascript")
        .args([
            "-e",
            r#"
on run argv
    set installCommand to "/bin/sh -c " & quoted form of item 1 of argv & " corvid-update"
    repeat with value in rest of argv
        set installCommand to installCommand & " " & quoted form of (contents of value)
    end repeat
    do shell script installCommand with administrator privileges
end run"#,
        ])
        .arg(include_str!("macos_authorized_install.sh"))
        .arg(app)
        .arg(incoming)
        .arg(expected)
        .arg(cdhash)
        .output()
        .context("requesting macOS administrator authorization")?;
    ensure!(
        result.status.success(),
        "macOS could not authorize the Corvid update: {}",
        String::from_utf8_lossy(&result.stderr).trim()
    );
    Ok(())
}
