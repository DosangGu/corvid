#[cfg(windows)]
pub use corvid_companion::launch::default_state_dir;

#[cfg(target_os = "linux")]
pub fn default_state_dir() -> anyhow::Result<std::path::PathBuf> {
    use std::env;
    use std::path::PathBuf;

    use anyhow::Context;

    let base = match env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => {
            let home = env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .context("set HOME, XDG_STATE_HOME, or choose a companion state directory")?;
            PathBuf::from(home).join(".local/state")
        }
    };
    Ok(base.join("corvid/companion"))
}
