//! Update a Finder-installed bundle without changing its outer directory identity.
use std::env;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use crate::{Installation, Product};

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
        installed.root.is_none() && installed.package.product == Product::Desktop,
        "the app belongs to a script installation; use its original installer"
    );
    if let Some(expected) = env::var_os("CORVID_EXPECT_APP_REVISION") {
        ensure!(
            expected == installed.package.source_revision.as_str(),
            "Corvid changed while the update was being prepared"
        );
    }
    ensure!(
        !app.as_os_str().as_bytes().contains(&b'\n'),
        "Corvid cannot update an app path containing a newline"
    );

    let incoming_cli = env::current_exe()?;
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
    let copy = Command::new("/usr/bin/ditto")
        .arg(incoming)
        .arg(&staged_app)
        .output()?;
    ensure!(
        copy.status.success(),
        "copying the verified app: {}",
        String::from_utf8_lossy(&copy.stderr).trim()
    );
    verify(&staged_app)?;
    ensure!(
        digest(app)? == expected,
        "Corvid changed while the update was being prepared"
    );

    let old = CString::new(app.join("Contents").as_os_str().as_bytes())?;
    let new = CString::new(staged_app.join("Contents").as_os_str().as_bytes())?;
    // Both directories are on the destination filesystem. The outer app retains its
    // name and file identity, including bookmarks made by Finder and the Dock.
    if unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            old.as_ptr(),
            libc::AT_FDCWD,
            new.as_ptr(),
            libc::RENAME_SWAP,
        )
    } != 0
    {
        return Err(io::Error::last_os_error()).context("exchanging Corvid app contents");
    }
    if let Err(error) = staging.close() {
        eprintln!("Installed Corvid. Could not remove the previous app contents: {error}");
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
