use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::hosts::{self, Profile, Subscriptions};
use crate::ui_signal::UiSignal;
use anyhow::Result;
use corvid_agent_deck::{connection::Client, focus};
use corvid_companion::{PresentationAddress, PresentationClient};
use corvid_presentation::ipc::Completion;
use corvid_protocol::wire::{CompanionConnection, PresentedRun, PresentedRunKey, RunAvailability};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

const MAX_RUNS: usize = 64;

pub(crate) struct Catalog {
    pub views: watch::Receiver<View>,
    requests: mpsc::Sender<Request>,
    pub completed: mpsc::Receiver<Completion>,
}

#[derive(Clone, Default)]
pub(crate) struct View {
    pub hosts: BTreeMap<Uuid, Arc<Host>>,
    pub message: Option<String>,
}

#[derive(Clone)]
pub(crate) struct Host {
    controller: Uuid,
    remote_epoch: Option<Uuid>,
    pub label: String,
    pub online: bool,
    pub runs: Vec<PresentedRun>,
    pub omitted: usize,
}

enum Request {
    Open(Uuid),
    Focus {
        id: Uuid,
        controller: Uuid,
        run: PresentedRunKey,
    },
}

impl Host {
    pub(crate) fn new(profile: &Profile) -> Self {
        Self {
            controller: profile.controller,
            label: profile.label.clone(),
            remote_epoch: None,
            online: false,
            runs: Vec::new(),
            omitted: 0,
        }
    }

    pub fn focus_target<'a>(
        &self,
        id: Uuid,
        run: &'a PresentedRun,
    ) -> Option<(Uuid, &'a PresentedRunKey)> {
        let key = run.key.as_ref()?;
        (self.online
            && run.availability == RunAvailability::Available as i32
            && Uuid::parse_str(&key.host_id).ok()? == id
            && Uuid::parse_str(&key.remote_epoch).ok()? == self.remote_epoch?
            && Uuid::parse_str(&key.agent_run_id).is_ok())
        .then_some((self.controller, key))
    }
}

impl Catalog {
    pub fn start(state: PathBuf, current: Uuid, ctx: UiSignal) -> Result<Self> {
        let profiles = hosts::discover(&state, current)?;
        let hosts = profiles
            .iter()
            .map(|(&id, profile)| (id, Arc::new(Host::new(profile))))
            .collect();
        let (publish, views) = watch::channel(View {
            hosts,
            message: None,
        });
        let (requests, incoming) = mpsc::channel(4);
        let (completions, completed) = mpsc::channel(4);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        std::thread::Builder::new()
            .name("corvid-agent-deck-hosts".into())
            .spawn(move || {
                runtime.block_on(run(
                    state,
                    current,
                    profiles,
                    publish,
                    incoming,
                    completions,
                    ctx,
                ));
            })?;
        Ok(Self {
            views,
            requests,
            completed,
        })
    }

    pub fn open(&self, host: Uuid) {
        let _ = self.requests.try_send(Request::Open(host));
    }
    pub fn focus(&self, id: Uuid, controller: Uuid, run: PresentedRunKey) -> bool {
        self.requests
            .try_send(Request::Focus {
                id,
                controller,
                run,
            })
            .is_ok()
    }
}

async fn run(
    state: PathBuf,
    current: Uuid,
    mut profiles: BTreeMap<Uuid, Profile>,
    publish: watch::Sender<View>,
    mut requests: mpsc::Receiver<Request>,
    completions: mpsc::Sender<Completion>,
    ctx: UiSignal,
) {
    let mut subscriptions = Subscriptions::default();
    let mut refresh = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = publish.closed() => break,
            _ = refresh.tick() => {
                if let Ok(next) = hosts::discover(&state, current)
                    && next != profiles
                {
                    publish.send_modify(|view| {
                        view.hosts.retain(|id, _| next.contains_key(id));
                        for (&id, profile) in &next {
                            if profiles.get(&id) != Some(profile) {
                                view.hosts.insert(id, Arc::new(Host::new(profile)));
                            }
                        }
                    });
                    profiles = next;
                    ctx.notify();
                }
                subscriptions.refresh(&profiles, |id, profile| {
                    monitor(id, profile.controller, profile.address, publish.clone(), ctx.clone())
                });
            }
            request = requests.recv() => {
                let Some(request) = request else { break };
                match request {
                    Request::Open(host) => {
                        if let Some(profile) = profiles.get(&host) {
                            let message = match Client::connect(host, profile.controller, false).await {
                                Ok(_) => None,
                                Err(_) => Some("Open that host's Agent Deck from Corvid Desktop first.".into()),
                            };
                            publish.send_modify(|view| view.message = message);
                        }
                    }
                    Request::Focus { id, controller, run } => {
                        let result = focus_captured_run(&publish, controller, run).await;
                        let (message, failed) = match result {
                            Ok(()) => ("Focused in Corvid Desktop.".into(), false),
                            Err(error) => (error.to_string(), true),
                        };
                        let _ = completions.try_send(Completion { id, message, failed, paired: None });
                    }
                }
                ctx.notify();
            }
        }
    }
}

async fn focus_captured_run(
    publish: &watch::Sender<View>,
    controller: Uuid,
    run: PresentedRunKey,
) -> Result<()> {
    let id = Uuid::parse_str(&run.host_id)?;
    let allowed = publish.borrow().hosts.get(&id).is_some_and(|host| {
        host.runs
            .iter()
            .any(|candidate| host.focus_target(id, candidate) == Some((controller, &run)))
    });
    anyhow::ensure!(allowed, "The agent is no longer available on this host.");
    focus::request(controller, run).await
}

async fn monitor(
    id: Uuid,
    controller: Uuid,
    address: <PresentationAddress as ToOwned>::Owned,
    publish: watch::Sender<View>,
    ctx: UiSignal,
) {
    loop {
        let _ = poll_host(id, controller, &address, &publish, &ctx).await;
        if publish.send_if_modified(|view| {
            let host = Arc::make_mut(view.hosts.get_mut(&id).expect("configured host"));
            std::mem::replace(&mut host.online, false)
        }) {
            ctx.notify();
        }
        tokio::select! {
            _ = publish.closed() => return,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

async fn poll_host(
    id: Uuid,
    controller: Uuid,
    address: &PresentationAddress,
    publish: &watch::Sender<View>,
    ctx: &UiSignal,
) -> Result<()> {
    let mut client = PresentationClient::connect(address).await?;
    anyhow::ensure!(
        client.session().host_id == id.to_string(),
        "unexpected monitor host"
    );
    loop {
        let slots =
            tokio::time::timeout(corvid_protocol::COMMAND_TIMEOUT, client.slots(None)).await??;
        anyhow::ensure!(
            slots.controller_id == controller.to_string(),
            "unexpected monitor controller"
        );
        let hidden_overflow = (slots.overflow_count as usize).saturating_sub(slots.overflow.len());
        let mut runs: Vec<_> = slots
            .slots
            .into_iter()
            .filter_map(|slot| slot.run)
            .chain(slots.overflow)
            .collect();
        let omitted = hidden_overflow + runs.len().saturating_sub(MAX_RUNS);
        runs.truncate(MAX_RUNS);
        let epoch = slots
            .remote_epoch
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok());
        let online = slots.connection == CompanionConnection::Ready as i32
            && slots
                .control_generation
                .as_deref()
                .is_some_and(|id| Uuid::parse_str(id).is_ok());
        if publish.send_if_modified(|view| {
            let host = view.hosts.get_mut(&id).expect("configured host");
            if host.remote_epoch == epoch
                && host.online == online
                && host.runs == runs
                && host.omitted == omitted
            {
                return false;
            }
            let host = Arc::make_mut(host);
            host.remote_epoch = epoch;
            host.online = online;
            host.runs = runs;
            host.omitted = omitted;
            true
        }) {
            ctx.notify();
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
