use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::ui_signal::UiSignal;
use anyhow::{Result, ensure};
use corvid_agent_deck::connection::Client;
use corvid_client::ControlEndpoint;
use corvid_companion::{Config, HostEndpoint};
use corvid_protocol::wire::AgentInfo;
use corvid_protocol::wire::snapshot_entity::Body;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

const MAX_HOSTS: usize = 8;
const MAX_RUNS: usize = 64;

pub(crate) struct Catalog {
    pub views: watch::Receiver<View>,
    open: mpsc::Sender<Uuid>,
}

#[derive(Clone, Default)]
pub(crate) struct View {
    pub hosts: BTreeMap<Uuid, Arc<Host>>,
    pub message: Option<String>,
}

#[derive(Clone)]
pub(crate) struct Host {
    pub label: String,
    pub online: bool,
    pub runs: Vec<AgentInfo>,
    pub omitted: usize,
}

impl Catalog {
    pub fn start(state: PathBuf, current: Uuid, ctx: UiSignal) -> Result<Self> {
        let profiles = discover(&state, current)?;
        let hosts = profiles
            .iter()
            .map(|(&id, profile)| {
                (
                    id,
                    Arc::new(Host {
                        label: label(&profile.endpoint),
                        online: false,
                        runs: Vec::new(),
                        omitted: 0,
                    }),
                )
            })
            .collect();
        let (publish, views) = watch::channel(View {
            hosts,
            message: None,
        });
        let (open, requests) = mpsc::channel(4);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        std::thread::Builder::new()
            .name("corvid-agent-deck-hosts".into())
            .spawn(move || {
                runtime.block_on(run(profiles, publish, requests, ctx));
            })?;
        Ok(Self { views, open })
    }

    pub fn open(&self, host: Uuid) {
        let _ = self.open.try_send(host);
    }
}

fn discover(state: &Path, current: Uuid) -> Result<BTreeMap<Uuid, Config>> {
    let main = Config::read_if_present(state)?
        .ok_or_else(|| anyhow::anyhow!("Agent Deck configuration is missing"))?;
    ensure!(main.host_id == current, "Agent Deck configuration changed");
    let root = state
        .parent()
        .filter(|parent| parent.ends_with("slot-board"))
        .filter(|_| {
            state.file_name().and_then(|part| part.to_str())
                == Some(current.simple().to_string().as_str())
        })
        .and_then(Path::parent)
        .unwrap_or(state);
    let mut profiles = BTreeMap::from([(current, main)]);
    if let Ok(Some(profile)) = Config::read_if_present(root) {
        profiles.entry(profile.host_id).or_insert(profile);
    }
    if let Ok(entries) = std::fs::read_dir(root.join("slot-board")) {
        let mut candidates: Vec<_> = entries
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
            if profiles.len() == MAX_HOSTS {
                break;
            }
            if let Ok(Some(profile)) = Config::read_if_present(&path)
                && profile.host_id == id
            {
                profiles.entry(id).or_insert(profile);
            }
        }
    }
    Ok(profiles)
}

fn label(endpoint: &HostEndpoint) -> String {
    match endpoint {
        HostEndpoint::Local { .. } => "Local".into(),
        HostEndpoint::Ssh { alias } => format!("SSH · {alias}"),
        HostEndpoint::Wsl { distribution } => format!("WSL · {distribution}"),
    }
}

async fn run(
    profiles: BTreeMap<Uuid, Config>,
    publish: watch::Sender<View>,
    mut requests: mpsc::Receiver<Uuid>,
    ctx: UiSignal,
) {
    let mut jobs = tokio::task::JoinSet::new();
    for (&id, profile) in &profiles {
        if let Ok(endpoint) = profile.control_endpoint() {
            jobs.spawn(monitor(id, endpoint, publish.clone(), ctx.clone()));
        }
    }
    loop {
        let host = tokio::select! {
            _ = publish.closed() => break,
            host = requests.recv() => match host {
                Some(host) => host,
                None => break,
            },
        };
        if let Some(profile) = profiles.get(&host) {
            let message = match Client::connect(host, profile.controller_id, false).await {
                Ok(_) => None,
                Err(_) => Some("Open that host's Agent Deck from Corvid Desktop first.".into()),
            };
            publish.send_modify(|view| view.message = message);
            ctx.notify();
        }
    }
}

async fn monitor(id: Uuid, endpoint: ControlEndpoint, publish: watch::Sender<View>, ctx: UiSignal) {
    let host = id.to_string();
    let controller = Uuid::new_v4().to_string();
    loop {
        if let Ok(mut watcher) = endpoint.watch(&controller, &host, || {}).await {
            loop {
                let mut runs: Vec<_> = watcher
                    .state()
                    .snapshot()
                    .entities()
                    .filter_map(|entity| {
                        let Some(Body::Agent(agent)) = &entity.body else {
                            return None;
                        };
                        agent
                            .parent_run_id
                            .is_none()
                            .then(|| agent.summary.clone())
                            .flatten()
                    })
                    .collect();
                runs.sort_by(|a, b| {
                    b.terminal_available
                        .cmp(&a.terminal_available)
                        .then_with(|| b.process_observed_unix_ms.cmp(&a.process_observed_unix_ms))
                        .then_with(|| a.agent_run_id.cmp(&b.agent_run_id))
                });
                let omitted = runs.len().saturating_sub(MAX_RUNS);
                runs.truncate(MAX_RUNS);
                publish.send_modify(|view| {
                    let host = Arc::make_mut(view.hosts.get_mut(&id).expect("configured host"));
                    host.online = true;
                    host.runs = runs;
                    host.omitted = omitted;
                });
                ctx.notify();
                if watcher.next_change().await.is_err() {
                    break;
                }
            }
        }
        publish.send_if_modified(|view| {
            let host = Arc::make_mut(view.hosts.get_mut(&id).expect("configured host"));
            std::mem::replace(&mut host.online, false)
        });
        ctx.notify();
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_shared_and_scoped_hosts_without_adopting_mismatched_directories() -> Result<()> {
        #[cfg(target_os = "linux")]
        let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
        #[cfg(windows)]
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("companion");
        let local = Uuid::new_v4();
        let remote = Uuid::new_v4();
        let scoped = root.join("slot-board").join(remote.simple().to_string());
        let wrong = root
            .join("slot-board")
            .join(Uuid::new_v4().simple().to_string());
        let endpoint = |alias: &str| HostEndpoint::Ssh {
            alias: alias.into(),
        };
        corvid_companion::initialize(&root, local, endpoint("first"), 8)?;
        corvid_companion::initialize(&scoped, remote, endpoint("second"), 8)?;
        corvid_companion::initialize(&wrong, Uuid::new_v4(), endpoint("misfiled"), 8)?;
        let profiles = discover(&scoped, remote)?;
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[&local].endpoint, endpoint("first"));
        assert_eq!(profiles[&remote].endpoint, endpoint("second"));
        assert!(discover(&scoped, local).is_err());
        Ok(())
    }
}
