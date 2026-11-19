//! Reconcile an activated package under its installer's publication lock.
//! A detached copy of the selected CLI retries busy images.
use std::collections::HashSet;
use std::fs;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_update::Installation;

mod packages;
mod processes;
#[cfg(windows)]
mod runtime_images;

pub(super) async fn run(retry: bool) -> Result<u32> {
    let installation = Installation::current()?;
    let root = dunce::canonicalize(
        installation
            .root
            .as_deref()
            .context("a managed installation is required")?,
    )?;
    let executable = dunce::canonicalize(std::env::current_exe()?)?;
    if retry {
        return retry_pending(&installation, &root, &executable).await;
    }
    let pending = match InstallLock::acquire(&root) {
        Ok(_lock) => {
            is_selected(&installation, &executable)?
                && match reconcile(&installation, &root, &executable, false).await {
                    Ok(pending) => pending,
                    Err(error) => {
                        eprintln!("Corvid cleanup pending: {error:#}");
                        true
                    }
                }
        }
        Err(_) => true,
    };
    if pending {
        println!(
            "Update applied. Cleanup is pending while previous Corvid sessions or files are in use."
        );
        let marker = installation
            .pending_retirement()
            .context("missing retirement marker path")?;
        fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(marker)?;
        Installation::start_retirement_worker(&executable)?;
    }
    Ok(0)
}

fn is_selected(installation: &Installation, executable: &Path) -> Result<bool> {
    Ok(installation
        .active_executable()?
        .is_some_and(|p| dunce::canonicalize(p).is_ok_and(|p| p == executable)))
}

async fn retry_pending(installation: &Installation, root: &Path, executable: &Path) -> Result<u32> {
    // One retry loop per installed version. The next generation can start before
    // the previous worker sees the new pointer and exits.
    let path = installation
        .pending_retirement()
        .context("missing retirement marker path")?;
    let worker = match fs::OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    if worker.try_lock().is_err() {
        return Ok(0);
    }
    let mut delay = 1;
    loop {
        tokio::time::sleep(Duration::from_secs(delay)).await;
        if !is_selected(installation, executable)? {
            break;
        }
        if let Ok(_lock) = InstallLock::acquire(root) {
            if !is_selected(installation, executable)? {
                break;
            }
            if matches!(
                reconcile(installation, root, executable, true).await,
                Ok(false)
            ) {
                drop(worker);
                fs::remove_file(path)?;
                return Ok(0);
            }
        }
        delay = (delay * 2).min(30);
    }
    drop(worker);
    let _ = fs::remove_file(path);
    Ok(0)
}

struct InstallLock {
    #[cfg(unix)]
    directory: PathBuf,
    #[cfg(windows)]
    _file: fs::File,
}

impl InstallLock {
    fn acquire(root: &Path) -> Result<Self> {
        let path = root.join(".install-lock");
        #[cfg(unix)]
        {
            fs::create_dir(&path)?;
            Ok(Self { directory: path })
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .share_mode(0)
                .open(path)?;
            Ok(Self { _file: file })
        }
    }
}

#[cfg(unix)]
impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.directory);
    }
}

async fn reconcile(
    installation: &Installation,
    root: &Path,
    executable: &Path,
    deferred: bool,
) -> Result<bool> {
    packages::empty_trash(root)?;
    let obsolete = packages::obsolete(installation, root)?;
    if obsolete.is_empty() {
        return Ok(false);
    }
    let runtime = crate::runtime_path(None)
        .ok()
        .map(|p| dunce::canonicalize(&p).unwrap_or(p));
    // No default runtime is needed for inspecting package directories on headless hosts.
    let runtime = runtime.as_deref().unwrap_or(Path::new(""));
    #[cfg(windows)]
    let copies = runtime_images::Copies::identify(installation, root, runtime, &obsolete)?;
    #[cfg(unix)]
    let _ = deferred;
    let mut pinned = HashSet::new();
    let running = processes::snapshot(&obsolete, runtime)?;
    for process in &running {
        let package = obsolete.iter().find(|dir| process.image.starts_with(dir));
        #[cfg(windows)]
        let owned = package.is_some() || copies.owners.contains_key(&process.image);
        #[cfg(unix)]
        let owned = package.is_some();
        if !owned {
            continue;
        }
        let result = if process.daemon() {
            processes::handoff(process, runtime).await
        } else if process.monitor() {
            processes::monitor(process, executable).await
        } else if process.image.file_stem() == Some(std::ffi::OsStr::new("corvid-agent-deck")) {
            processes::deck(process, executable).await
        } else {
            #[cfg(windows)]
            if let Some(dir) = package
                && deferred
            {
                processes::legacy_desktop(process, dir, executable).await
            } else {
                Ok(())
            }
            #[cfg(unix)]
            Ok(())
        };
        if let Err(error) = result {
            eprintln!("Corvid process replacement pending: {error:#}");
        }
    }
    // Repeat enumeration after handoffs. PTY hosts and supervised instances can
    // outlive a daemon; never unlink a Unix image merely because unlink succeeds.
    let running = processes::snapshot(&obsolete, runtime)?;
    #[cfg(windows)]
    let copies_pending = copies.sweep(&running, runtime, &mut pinned).await?;
    #[cfg(unix)]
    let copies_pending = false;
    for process in running {
        if let Some(dir) = obsolete.iter().find(|d| process.image.starts_with(d)) {
            pinned.insert(dir.clone());
        }
    }
    let mut pending = copies_pending || !pinned.is_empty();
    for package in obsolete {
        if pinned.contains(&package) {
            continue;
        }
        ensure!(
            is_selected(installation, executable)?,
            "the selected version changed during cleanup"
        );
        match packages::remove(&package, root) {
            Ok(true) => println!("Removed obsolete Corvid package {}.", package.display()),
            Ok(false) => eprintln!(
                "Preserved package with unrecognized files: {}",
                package.display()
            ),
            Err(error) => {
                pending = true;
                eprintln!("Corvid package cleanup pending: {error:#}");
            }
        }
    }
    Ok(pending)
}
