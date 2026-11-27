use anyhow::{Context, Result};

use super::Directory;

pub(super) fn read_with_backup<T>(
    directory: &Directory,
    file: &str,
    backup: &str,
    max_bytes: u64,
    decode: impl Fn(&[u8]) -> Result<T>,
    require_schema: impl Fn(&[u8]) -> Result<()>,
    invalid_message: &str,
) -> Result<Option<T>> {
    if let Some(bytes) = directory.read_private_file(file, max_bytes)? {
        return match decode(&bytes) {
            Ok(value) => Ok(Some(value)),
            Err(_) => {
                // Never reinterpret or overwrite a future schema as an older one.
                require_schema(&bytes)?;
                directory
                    .read_private_file(backup, max_bytes)?
                    .map(|backup_bytes| decode(&backup_bytes))
                    .transpose()?
                    .map(Some)
                    .with_context(|| invalid_message.to_owned())
            }
        };
    }

    directory
        .read_private_file(backup, max_bytes)?
        .map(|backup_bytes| decode(&backup_bytes))
        .transpose()
}

pub(super) fn load_for_write<T: Default>(
    directory: &Directory,
    file: &str,
    backup: &str,
    corrupt: &str,
    max_bytes: u64,
    decode: impl Fn(&[u8]) -> Result<T>,
    require_schema: impl Fn(&[u8]) -> Result<()>,
) -> Result<T> {
    if let Some(bytes) = directory.read_private_file(file, max_bytes)? {
        return match decode(&bytes) {
            Ok(value) => {
                directory.replace_private_file(backup, &bytes)?;
                Ok(value)
            }
            Err(_) => {
                require_schema(&bytes)?;
                directory.replace_private_file(corrupt, &bytes)?;
                Ok(directory
                    .read_private_file(backup, max_bytes)?
                    .and_then(|backup_bytes| decode(&backup_bytes).ok())
                    .unwrap_or_default())
            }
        };
    }

    Ok(directory
        .read_private_file(backup, max_bytes)?
        .and_then(|backup_bytes| decode(&backup_bytes).ok())
        .unwrap_or_default())
}
