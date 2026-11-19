use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
use corvid_update::{Installation, Package};

/// Only accept canonical version directories from this product and installer.
pub(super) fn obsolete(installation: &Installation, root: &Path) -> Result<Vec<PathBuf>> {
    let versions = root.join("versions");
    ensure!(
        dunce::canonicalize(&versions)? == versions,
        "redirected versions directory"
    );
    let mut packages = Vec::new();
    for entry in fs::read_dir(&versions)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == installation.package.version
            || !corvid_update::version(name).is_ok_and(|v| v.to_string() == name)
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        let path = entry.path();
        if dunce::canonicalize(&path)? != path {
            continue;
        }
        let metadata = path.join("package.json");
        if !metadata.is_file() || dunce::canonicalize(&metadata)? != metadata {
            continue;
        }
        let Ok(package) = serde_json::from_slice::<Package>(&fs::read(metadata)?) else {
            continue;
        };
        if package.schema_version == 1
            && package.version == name
            && package.product == installation.package.product
            && package.target == installation.package.target
            && path.join("native-package").exists() == installation.is_windows_setup()
        {
            packages.push(path);
        }
    }
    Ok(packages)
}

/// Check the whole tree before detaching it. Extra user files and redirected subtrees
/// keep their directory intact. Retirements interrupted after rename resume from trash.
pub(super) fn remove(package: &Path, root: &Path) -> Result<bool> {
    let manifest = package.join("files.sha256");
    ensure!(
        dunce::canonicalize(&manifest)? == manifest,
        "redirected package manifest"
    );
    let mut owned = HashSet::from([PathBuf::from("files.sha256")]);
    for line in fs::read_to_string(manifest)?.lines() {
        let (digest, name) = line
            .split_once("  ")
            .ok_or_else(|| anyhow::anyhow!("invalid package manifest"))?;
        let name = Path::new(name);
        ensure!(
            digest.len() == 64
                && digest.bytes().all(|b| b.is_ascii_hexdigit())
                && name
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "invalid package file entry"
        );
        owned.insert(name.to_owned());
    }
    let mut dirs = vec![package.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if dunce::canonicalize(&path)? != path {
                return Ok(false);
            }
            if kind.is_dir() {
                dirs.push(path);
            } else if !kind.is_file() || !owned.contains(path.strip_prefix(package)?) {
                return Ok(false);
            }
        }
    }
    let trash = root.join(".retiring");
    fs::create_dir_all(&trash)?;
    ensure!(
        dunce::canonicalize(&trash)? == trash,
        "redirected retirement directory"
    );
    let dest = trash.join(uuid::Uuid::new_v4().to_string());
    fs::rename(package, &dest)?;
    fs::remove_dir_all(dest)?;
    Ok(true)
}

pub(super) fn empty_trash(root: &Path) -> Result<()> {
    let trash = root.join(".retiring");
    if !trash.try_exists()? {
        return Ok(());
    }
    ensure!(
        dunce::canonicalize(&trash)? == trash,
        "redirected retirement directory"
    );
    for entry in fs::read_dir(&trash)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|n| uuid::Uuid::parse_str(n).is_ok())
            && dunce::canonicalize(entry.path())? == entry.path()
        {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}
