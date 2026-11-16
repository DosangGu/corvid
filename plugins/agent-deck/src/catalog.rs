use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::hosts::{self, Profile, Subscriptions};
use crate::ui_signal::UiSignal;
use anyhow::Result;
use corvid_agent_deck::connection::Client;
use corvid_companion::{PresentationAddress, PresentationClient};
use corvid_protocol::wire::{CompanionConnection, PresentedRun};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

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
    pub runs: Vec<PresentedRun>,
    pub omitted: usize,
}

impl Catalog {
    pub fn start(state: PathBuf, current: Uuid, ctx: UiSignal) -> Result<Self> {
        let profiles = hosts::discover(&state, current)?;
        let hosts = profiles
            .iter()
            .map(|(&id, profile)| {
                (
                    id,
                    Arc::new(Host {
                        label: profile.label.clone(),
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
                runtime.block_on(run(state, current, profiles, publish, requests, ctx));
            })?;
        Ok(Self { views, open })
    }

    pub fn open(&self, host: Uuid) {
        let _ = self.open.try_send(host);
    }
}

async fn run(
    state: PathBuf,
    current: Uuid,
    mut profiles: BTreeMap<Uuid, Profile>,
    publish: watch::Sender<View>,
    mut requests: mpsc::Receiver<Uuid>,
    ctx: UiSignal,
) {
    let mut subscriptions = Subscriptions::default();
    let mut refresh = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = publish.closed() => break,
            _ = refresh.tick() => {
                if let Ok(next) = hosts::discover(&state, current) {
                    profiles = next;
                }
                publish.send_modify(|view| {
                    view.hosts.retain(|id, _| profiles.contains_key(id));
                    for (&id, profile) in &profiles {
                        let host = view.hosts.entry(id).or_insert_with(|| Arc::new(Host {
                            label: profile.label.clone(), online: false, runs: Vec::new(), omitted: 0,
                        }));
                        Arc::make_mut(host).label = profile.label.clone();
                    }
                });
                subscriptions.refresh(&profiles, |id, profile| {
                    monitor(id, profile.controller, profile.address, publish.clone(), ctx.clone())
                });
                ctx.notify();
            }
            host = requests.recv() => {
                let Some(host) = host else { break };
                if let Some(profile) = profiles.get(&host) {
                    let message = match Client::connect(host, profile.controller, false).await {
                        Ok(_) => None,
                        Err(_) => Some("Open that host's Agent Deck from Corvid Desktop first.".into()),
                    };
                    publish.send_modify(|view| view.message = message);
                    ctx.notify();
                }
            }
        }
    }
}

async fn monitor(
    id: Uuid,
    controller: Uuid,
    address: <PresentationAddress as ToOwned>::Owned,
    publish: watch::Sender<View>,
    ctx: UiSignal,
) {
    loop {
        let result = async {
            let mut client = PresentationClient::connect(&address).await?;
            anyhow::ensure!(
                client.session().host_id == id.to_string(),
                "unexpected monitor host"
            );
            let slots = client.slots(None).await?;
            anyhow::ensure!(
                slots.controller_id == controller.to_string(),
                "unexpected monitor controller"
            );
            let hidden_overflow =
                (slots.overflow_count as usize).saturating_sub(slots.overflow.len());
            let mut runs: Vec<_> = slots
                .slots
                .into_iter()
                .filter_map(|slot| slot.run)
                .chain(slots.overflow)
                .collect();
            let omitted = hidden_overflow + runs.len().saturating_sub(MAX_RUNS);
            runs.truncate(MAX_RUNS);
            publish.send_modify(|view| {
                let host = Arc::make_mut(view.hosts.get_mut(&id).expect("configured host"));
                host.online = slots.connection == CompanionConnection::Ready as i32;
                host.runs = runs;
                host.omitted = omitted;
            });
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if result.is_err() {
            publish.send_if_modified(|view| {
                let host = Arc::make_mut(view.hosts.get_mut(&id).expect("configured host"));
                std::mem::replace(&mut host.online, false)
            });
        }
        ctx.notify();
        tokio::select! {
            _ = publish.closed() => return,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}
