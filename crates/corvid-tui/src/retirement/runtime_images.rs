//! Windows Desktop releases before packaged daemons ran in place used private copies.
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
use corvid_protocol::windows::StateDirectory;
use sha2::{Digest, Sha256};

use super::processes::Running;

fn hash(path: &Path) -> Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 32 * 1024];
    loop {
        let count = std::io::Read::read(&mut file, &mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(digest.finalize().to_vec())
}

pub(super) struct Copies {
    /// Match bytes with a runtime from this exact package set before acting on a legacy copy.
    pub owners: HashMap<PathBuf, Vec<PathBuf>>,
}

impl Copies {
    pub fn identify(
        installation: &corvid_update::Installation,
        root: &Path,
        runtime: &Path,
        packages: &[PathBuf],
    ) -> Result<Self> {
        let mut owners = HashMap::new();
        // Legacy copies have no installation receipt. Only the standard per-user
        // Desktop installation can claim its matching payloads in the default runtime.
        let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) else {
            return Ok(Self { owners });
        };
        let standard = if installation.is_windows_setup() {
            local.join("Programs/Corvid")
        } else {
            local.join("Corvid/desktop")
        };
        if installation.package.product != corvid_update::Product::Desktop
            || dunce::canonicalize(standard).ok().as_deref() != Some(root)
            || dunce::canonicalize(local.join("Corvid/runtime"))
                .ok()
                .as_deref()
                != Some(runtime)
            || !runtime.is_dir()
            || packages.is_empty()
        {
            return Ok(Self { owners });
        }
        let _private = StateDirectory::open(runtime)?;
        ensure!(
            dunce::canonicalize(runtime)? == runtime,
            "redirected runtime directory"
        );
        let mut runtimes = HashMap::<u64, Vec<(PathBuf, Vec<u8>)>>::new();
        for dir in packages {
            let binary = dir.join("corvid.exe");
            if binary.is_file() && dunce::canonicalize(&binary)? == binary {
                runtimes
                    .entry(fs::metadata(&binary)?.len())
                    .or_default()
                    .push((dir.clone(), hash(&binary)?));
            }
        }
        for entry in fs::read_dir(runtime)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let host_copy = name
                .strip_prefix("pty-host-")
                .and_then(|s| s.strip_suffix(".exe"))
                .is_some_and(|s| s.len() == 32 && uuid::Uuid::parse_str(s).is_ok());
            if !(host_copy
                || matches!(
                    name,
                    "desktop-daemon.exe" | "desktop-daemon-replacement.exe"
                ))
            {
                continue;
            }
            let path = entry.path();
            if !entry.file_type()?.is_file() || dunce::canonicalize(&path)? != path {
                continue;
            }
            let Some(candidates) = runtimes.get(&entry.metadata()?.len()) else {
                continue;
            };
            let digest = hash(&path)?;
            let matched: Vec<_> = candidates
                .iter()
                .filter(|(_, h)| h == &digest)
                .map(|(p, _)| p.clone())
                .collect();
            if !matched.is_empty() {
                owners.insert(path, matched);
            }
        }
        Ok(Self { owners })
    }

    pub async fn sweep(
        &self,
        running: &[Running],
        runtime: &Path,
        pinned: &mut HashSet<PathBuf>,
    ) -> Result<bool> {
        let mut pending = false;
        if self.owners.is_empty() {
            return Ok(false);
        }
        // A daemon may start another host from its private image between snapshots.
        let dir = StateDirectory::open(runtime)?;
        let _startup = dir.lock_startup()?;
        let offline = dir.lock_instance().ok();
        let daemon = if offline.is_some() {
            None
        } else {
            Some(crate::windows::daemon_image(runtime).await?)
        };
        let active_digest = daemon.as_deref().map(hash).transpose()?;
        let mut host_owners = HashSet::new();
        for p in running.iter().filter(|p| {
            p.image.parent() == Some(runtime)
                && p.image
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("pty-host-"))
        }) {
            if let Some(owners) = self.owners.get(&p.image) {
                host_owners.extend(owners.iter().cloned());
                pinned.extend(owners.iter().cloned());
            }
        }
        for (image, owners) in &self.owners {
            let live = running.iter().any(|p| &p.image == image);
            let host_image = image
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("pty-host-"));
            let reserved = host_image
                && active_digest
                    .as_ref()
                    .is_some_and(|h| hash(image).map_or(true, |digest| &digest == h));
            if live || reserved || (!host_image && owners.iter().any(|d| host_owners.contains(d))) {
                // Legacy hooks can still invoke the image that launched the session.
                pinned.extend(owners.iter().cloned());
                pending = true;
            } else if let Err(error) = fs::remove_file(image)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                pinned.extend(owners.iter().cloned());
                pending = true;
            }
        }
        Ok(pending)
    }
}
