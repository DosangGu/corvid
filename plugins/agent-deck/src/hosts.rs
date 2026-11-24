use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use corvid_async::task::JoinHandle;
use corvid_companion::{Config, HostEndpoint, PresentationAddress};
use uuid::Uuid;

pub const MAX_HOSTS: usize = 8;

#[derive(Clone, PartialEq, Eq)]
pub struct Profile {
    pub controller: Uuid,
    pub address: <PresentationAddress as ToOwned>::Owned,
    pub label: String,
}

pub use corvid_agent_deck::scope::profile_root as root;

pub fn discover(state: &Path, current: Uuid) -> Result<BTreeMap<Uuid, Profile>> {
    discover_with_limit(state, current, MAX_HOSTS)
}

pub fn discover_all(state: &Path, current: Uuid) -> Result<BTreeMap<Uuid, Profile>> {
    discover_with_limit(state, current, usize::MAX)
}

fn discover_with_limit(
    state: &Path,
    current: Uuid,
    limit: usize,
) -> Result<BTreeMap<Uuid, Profile>> {
    let main = Config::read_if_present(state)?.context("Agent Deck configuration is missing")?;
    ensure!(main.host_id == current, "Agent Deck configuration changed");
    let root = root(state, current);
    let mut profiles = BTreeMap::new();
    insert(&mut profiles, state, root, main)?;
    if let Ok(Some(profile)) = Config::read_if_present(root) {
        insert(&mut profiles, root, root, profile)?;
    }
    if let Ok(entries) = std::fs::read_dir(root.join("slot-board")) {
        let mut candidates: Vec<(Uuid, PathBuf)> = entries
            .take(256)
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| {
                let id = Uuid::parse_str(entry.file_name().to_str()?).ok()?;
                Some((id, entry.path()))
            })
            .collect();
        candidates.sort_unstable_by_key(|(id, _)| *id);
        for (id, path) in candidates {
            if profiles.len() == limit {
                break;
            }
            if let Ok(Some(profile)) = Config::read_if_present(&path)
                && profile.host_id == id
            {
                // A partially configured sibling must not interrupt healthy hosts.
                let _ = insert(&mut profiles, &path, root, profile);
            }
        }
    }
    Ok(profiles)
}

fn insert(
    profiles: &mut BTreeMap<Uuid, Profile>,
    state: &Path,
    root: &Path,
    config: Config,
) -> Result<()> {
    if profiles.contains_key(&config.host_id) {
        return Ok(());
    }
    let address = corvid_companion::monitor::saved_address(state, &config, state == root)?;
    let label = match &config.endpoint {
        HostEndpoint::Local { .. } => "Local".into(),
        HostEndpoint::Ssh { alias } => format!("SSH · {alias}"),
        HostEndpoint::Wsl { distribution } => format!("WSL · {distribution}"),
    };
    profiles.insert(
        config.host_id,
        Profile {
            controller: config.controller_id,
            address,
            label,
        },
    );
    Ok(())
}

/// Keep readers attached as verified host scopes appear or change while Deck is open.
#[derive(Default)]
pub struct Subscriptions {
    tasks: BTreeMap<Uuid, (Profile, JoinHandle<()>)>,
}

impl Subscriptions {
    pub fn refresh<F, Fut>(&mut self, profiles: &BTreeMap<Uuid, Profile>, mut start: F)
    where
        F: FnMut(Uuid, Profile) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.tasks.retain(|id, (previous, task)| {
            let keep = profiles.get(id) == Some(previous) && !task.is_finished();
            if !keep {
                task.abort();
            }
            keep
        });
        for (&id, profile) in profiles {
            self.tasks.entry(id).or_insert_with(|| {
                (
                    profile.clone(),
                    corvid_async::task::spawn(start(id, profile.clone())),
                )
            });
        }
    }
}

impl Drop for Subscriptions {
    fn drop(&mut self) {
        for (_, task) in self.tasks.values() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configure(path: &Path) -> Result<()> {
        let config = Config::read_if_present(path)?.context("fixture config")?;
        #[cfg(windows)]
        let address = config.controller_id.simple().to_string();
        #[cfg(not(windows))]
        let address = path.join("ipc");
        corvid_companion::monitor::Scope {
            state: path.to_owned(),
            config,
            address,
        }
        .configure(&corvid_client::ControlEndpoint::Local(path.join("unused")))
    }

    #[compio::test]
    async fn subscribes_to_a_host_connected_after_deck_start_without_restarting_existing_readers()
    -> Result<()> {
        #[cfg(not(windows))]
        let temporary = tempfile::tempdir_in(std::env::home_dir().unwrap())?;
        #[cfg(windows)]
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("companion");
        let local = Uuid::new_v4();
        let remote = Uuid::new_v4();
        let endpoint = |alias: &str| HostEndpoint::Ssh {
            alias: alias.into(),
        };
        corvid_companion::initialize(&root, local, endpoint("first"), 8)?;
        configure(&root)?;
        let mut subscriptions = Subscriptions::default();
        let (started, mut readers) = corvid_async::mpsc::unbounded_channel();
        let start = |id, _| {
            let started = started.clone();
            async move {
                started.send(id).unwrap();
                std::future::pending::<()>().await;
            }
        };
        subscriptions.refresh(&discover(&root, local)?, start);
        assert_eq!(readers.recv().await, Some(local));
        let scoped = root.join("slot-board").join(remote.simple().to_string());
        corvid_companion::initialize(&scoped, remote, endpoint("second"), 8)?;
        configure(&scoped)?;
        let wrong = root
            .join("slot-board")
            .join(Uuid::new_v4().simple().to_string());
        corvid_companion::initialize(&wrong, Uuid::new_v4(), endpoint("misfiled"), 8)?;
        let profiles = discover(&root, local)?;
        assert_eq!(profiles.len(), 2);
        subscriptions.refresh(&profiles, start);
        assert_eq!(readers.recv().await, Some(remote));
        subscriptions.refresh(&discover(&scoped, remote)?, start);
        corvid_async::task::yield_now().await;
        assert!(readers.try_recv().is_err());
        assert!(discover(&scoped, local).is_err());
        Ok(())
    }
}
