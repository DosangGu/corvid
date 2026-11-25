use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use corvid_core::agent::Provider;
use serde_json::{Map, Value};
use tempfile::NamedTempFile;

const HOOK_KEY: &str = "corvid-observer";
const MAX_HOOKS_BYTES: u64 = 256 * 1024;

/// Install Corvid's observer entry into the provider's global hooks file.
pub fn install_hooks(provider: Provider) -> Result<PathBuf> {
    let path = global_hooks_path(provider)?;
    let settings = crate::observer_settings(provider)?;
    let entry = settings
        .get(HOOK_KEY)
        .cloned()
        .context("generated observer settings lack the corvid-observer entry")?;
    install_entry_at(&path, entry)?;
    Ok(path)
}

/// Remove Corvid's observer entry from the provider's global hooks file.
pub fn uninstall_hooks(provider: Provider) -> Result<Option<PathBuf>> {
    let path = global_hooks_path(provider)?;
    if uninstall_entry_at(&path)? {
        Ok(Some(path))
    } else {
        Ok(None)
    }
}

fn global_hooks_path(provider: Provider) -> Result<PathBuf> {
    if provider != Provider::Antigravity {
        bail!(
            "--install/--uninstall is only supported for antigravity; run `corvid hooks <provider>` to print settings for manual merge"
        );
    }
    let home = env::home_dir().context("home directory unavailable")?;
    ensure!(
        home.is_absolute(),
        "home directory must be an absolute path"
    );
    Ok(home.join(".gemini").join("config").join("hooks.json"))
}

fn install_entry_at(path: &Path, entry: Value) -> Result<()> {
    let mut root = read_hooks_object(path)?.unwrap_or_default();
    root.insert(HOOK_KEY.to_owned(), entry);
    write_hooks_object(path, &root)
}

fn uninstall_entry_at(path: &Path) -> Result<bool> {
    let Some(mut root) = read_hooks_object(path)? else {
        return Ok(false);
    };
    if root.remove(HOOK_KEY).is_none() {
        return Ok(false);
    }
    write_hooks_object(path, &root)?;
    Ok(true)
}

fn read_hooks_object(path: &Path) -> Result<Option<Map<String, Value>>> {
    let file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("opening {}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("inspecting {}", path.display()))?;
    ensure!(
        metadata.is_file(),
        "{} is not a regular file",
        path.display()
    );
    ensure!(
        metadata.len() <= MAX_HOOKS_BYTES,
        "{} exceeds the {} byte hook config limit",
        path.display(),
        MAX_HOOKS_BYTES
    );
    let mut bytes = Vec::new();
    file.take(MAX_HOOKS_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= MAX_HOOKS_BYTES,
        "{} exceeds the {} byte hook config limit",
        path.display(),
        MAX_HOOKS_BYTES
    );
    let text =
        std::str::from_utf8(&bytes).with_context(|| format!("{} must be UTF-8", path.display()))?;
    let trimmed = text.strip_prefix('\u{feff}').unwrap_or(text).trim();
    if trimmed.is_empty() {
        return Ok(Some(Map::new()));
    }
    let Value::Object(object) = serde_json::from_str(trimmed)
        .with_context(|| format!("parsing {} as JSON", path.display()))?
    else {
        bail!("{} must contain a top-level JSON object", path.display());
    };
    Ok(Some(object))
}

fn write_hooks_object(path: &Path, root: &Map<String, Value>) -> Result<()> {
    let parent = path
        .parent()
        .context("hooks path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let mut encoded = serde_json::to_vec_pretty(root)?;
    encoded.push(b'\n');

    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(&encoded)?;
    temporary.flush()?;
    #[cfg(unix)]
    if let Ok(existing) = fs::metadata(path) {
        let _ = fs::set_permissions(temporary.path(), existing.permissions());
    }
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("updating {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn install_and_uninstall_preserve_unrelated_hooks_and_reject_corrupt_files() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join(".gemini/config/hooks.json");

        // Fresh install creates parent directories and writes only corvid-observer.
        let entry = json!({
            "SessionStart": [{"type": "command", "command": "exec '/opt/corvid' observe --provider antigravity", "timeout": 1}]
        });
        install_entry_at(&path, entry.clone()).unwrap();
        let stored: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored, json!({ "corvid-observer": entry }));

        // Preserves other user hooks and UTF-8 BOM inputs, and updates idempotently.
        let with_user = json!({
            "user-linter": { "PostToolUse": [] },
            "corvid-observer": { "SessionStart": [] }
        });
        let mut bom_bytes = b"\xef\xbb\xbf".to_vec();
        bom_bytes.extend_from_slice(&serde_json::to_vec_pretty(&with_user).unwrap());
        fs::write(&path, bom_bytes).unwrap();

        install_entry_at(&path, entry.clone()).unwrap();
        let merged: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(merged["user-linter"], json!({ "PostToolUse": [] }));
        assert_eq!(merged["corvid-observer"], entry);

        // Uninstall removes only corvid-observer and reports when already absent.
        assert!(uninstall_entry_at(&path).unwrap());
        assert!(!uninstall_entry_at(&path).unwrap());
        let after: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(after, json!({ "user-linter": { "PostToolUse": [] } }));

        // Malformed JSON or non-object top-level values fail without clobbering.
        fs::write(&path, b"{broken").unwrap();
        assert!(install_entry_at(&path, entry.clone()).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"{broken");

        fs::write(&path, b"[]").unwrap();
        assert!(install_entry_at(&path, entry).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"[]");
    }

    #[test]
    fn install_hooks_rejects_unsupported_providers() {
        assert!(install_hooks(Provider::Claude).is_err());
        assert!(uninstall_hooks(Provider::Codex).is_err());
    }
}
