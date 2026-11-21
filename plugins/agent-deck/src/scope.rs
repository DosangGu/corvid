use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_async::time::{sleep, timeout};
use corvid_companion::Config;
use corvid_companion::monitor::{Scope, saved_address};
#[cfg(not(windows))]
use corvid_protocol::unix::RuntimeDir as StateDirectory;
#[cfg(windows)]
use corvid_protocol::windows::StateDirectory;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const OWNER_FILE: &str = "owner.json";
const VERSION: u32 = 1;

#[derive(Deserialize, Serialize)]
struct Owner {
    schema_version: u32,
    host: Uuid,
    controller: Uuid,
}

pub fn profile_root(state: &Path, host: Uuid) -> &Path {
    state
        .parent()
        .filter(|parent| parent.ends_with("slot-board"))
        .filter(|_| {
            state.file_name().and_then(|name| name.to_str())
                == Some(host.simple().to_string().as_str())
        })
        .and_then(Path::parent)
        .unwrap_or(state)
}

/// Retain one existing companion identity for the Deck's activation endpoint
/// and preferences. Each host continues to own its own monitor and slots.
pub async fn shared_scope(preferred: Scope) -> Result<Scope> {
    let root = profile_root(&preferred.state, preferred.config.host_id);
    let directory = StateDirectory::create_all(&root.join("agent-deck-launch"))?;
    let _lock = timeout(corvid_protocol::COMMAND_TIMEOUT, async {
        loop {
            match directory.lock_storage() {
                Ok(lock) => break Ok(lock),
                Err(error) if retry_lock(&error) => sleep(Duration::from_millis(25)).await,
                Err(error) => break Err(error),
            }
        }
    })
    .await
    .context("waiting for the shared Agent Deck selection")??;

    if let Some(bytes) = directory.read_private_file(OWNER_FILE, 1024)? {
        let owner: Owner =
            serde_json::from_slice(&bytes).context("reading the saved Agent Deck selection")?;
        ensure!(
            owner.schema_version == VERSION,
            "unsupported Agent Deck selection schema"
        );
        if owner.matches(&preferred.config) {
            return Ok(preferred);
        }
        let child = root
            .join("slot-board")
            .join(owner.host.simple().to_string());
        for state in [root, child.as_path()] {
            if let Some(config) = Config::read_if_present(state)?
                && owner.matches(&config)
            {
                let address = saved_address(state, &config, state == root)?;
                return Ok(Scope {
                    state: state.to_owned(),
                    address,
                    config,
                });
            }
        }
    }

    let owner = Owner {
        schema_version: VERSION,
        host: preferred.config.host_id,
        controller: preferred.config.controller_id,
    };
    directory.replace_private_file(OWNER_FILE, &serde_json::to_vec(&owner)?)?;
    Ok(preferred)
}

impl Owner {
    fn matches(&self, config: &Config) -> bool {
        self.host == config.host_id && self.controller == config.controller_id
    }
}

fn retry_lock(error: &anyhow::Error) -> bool {
    #[cfg(target_os = "macos")]
    if error.downcast_ref::<nix::errno::Errno>() == Some(&nix::errno::Errno::ENOENT) {
        // Concurrent first launches can race while creating the lock file.
        return true;
    }

    #[cfg(not(windows))]
    return error.downcast_ref::<nix::errno::Errno>() == Some(&nix::errno::Errno::EWOULDBLOCK);

    #[cfg(windows)]
    return error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        error.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use corvid_client::ControlEndpoint;

    #[test]
    fn simultaneous_launches_for_different_hosts_choose_one_controller() -> Result<()> {
        #[cfg(not(windows))]
        let temporary = tempfile::tempdir_in(std::env::home_dir().context("home")?)?;
        #[cfg(windows)]
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("companion");
        let first_host = Uuid::new_v4();
        let second_host = Uuid::new_v4();
        let endpoint = ControlEndpoint::Local(temporary.path().join("runtime"));
        let first = Scope::prepare(&root, &endpoint, first_host, "main")?;
        let second = Scope::prepare(&root, &endpoint, second_host, "main")?;
        first.configure(&endpoint)?;
        second.configure(&endpoint)?;
        let barrier = std::sync::Barrier::new(2);
        let select = |scope| {
            barrier.wait();
            run_selection(scope)
        };
        let (first, second) = std::thread::scope(|threads| {
            let first = threads.spawn(|| select(first));
            let second = threads.spawn(|| select(second));
            Ok::<_, anyhow::Error>((first.join().unwrap()?, second.join().unwrap()?))
        })?;
        assert_eq!(first.state, second.state);
        assert_eq!(first.config.controller_id, second.config.controller_id);

        // A newly configured host must reuse the choice after the launchers exit.
        let later = Scope::prepare(&root, &endpoint, Uuid::new_v4(), "main")?;
        let later = run_selection(later)?;
        assert_eq!(later.config.controller_id, first.config.controller_id);
        assert_eq!(later.address, first.address);
        Ok(())
    }

    fn run_selection(scope: Scope) -> Result<Scope> {
        compio::runtime::Runtime::new()?.block_on(shared_scope(scope))
    }
}
