use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::hosts::{self, Profile, Subscriptions};
use crate::parent::DesktopLink;
use crate::ui_signal::UiSignal;
use anyhow::Result;
use corvid_agent_deck::connection::Client;
use corvid_async::{mpsc, watch};
use corvid_companion::{
    MAX_TRACKED, PRESENTATION_PAGE_SIZE, PresentationAddress, PresentationClient, RunKey,
};
use corvid_presentation::desktop_focus::{self, EndpointUnavailable};
use corvid_presentation::ipc::{Completion, Display, Link};
use corvid_presentation::model::Snapshot;
use corvid_protocol::ProtocolFault;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    CompanionConnection, GetPresentationSlots, PresentationSlots, PresentedRun, PresentedRunKey,
    RunAvailability,
};
use uuid::Uuid;

pub(crate) const HOST_PREVIEW_LIMIT: usize = 64;

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
    pub loaded: bool,
    pub runs: Vec<PresentedRun>,
    pub pins: BTreeSet<RunKey>,
}

enum Request {
    Open(Uuid),
    Focus {
        id: Uuid,
        controller: Uuid,
        run: PresentedRunKey,
    },
    FocusCurrent {
        id: Uuid,
        snapshot: Arc<Snapshot>,
        run: PresentedRunKey,
    },
}

struct CurrentDesktop {
    view: watch::Receiver<Display>,
    legacy: DesktopLink,
}

impl Host {
    pub(crate) fn new(profile: &Profile) -> Self {
        Self {
            controller: profile.controller,
            label: profile.label.clone(),
            remote_epoch: None,
            online: false,
            loaded: false,
            runs: Vec::new(),
            pins: BTreeSet::new(),
        }
    }

    pub fn matches_epoch(&self, epoch: Option<&str>) -> bool {
        self.remote_epoch == epoch.and_then(|epoch| Uuid::parse_str(epoch).ok())
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
    pub fn start(
        state: PathBuf,
        current: Uuid,
        ctx: UiSignal,
        view: watch::Receiver<Display>,
        legacy: DesktopLink,
    ) -> Result<Self> {
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
                    (state, current),
                    profiles,
                    publish,
                    incoming,
                    completions,
                    ctx,
                    CurrentDesktop { view, legacy },
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

    pub fn focus_current(&self, id: Uuid, snapshot: Arc<Snapshot>, run: PresentedRunKey) -> bool {
        self.requests
            .try_send(Request::FocusCurrent { id, snapshot, run })
            .is_ok()
    }
}

async fn run(
    (state, current): (PathBuf, Uuid),
    mut profiles: BTreeMap<Uuid, Profile>,
    publish: watch::Sender<View>,
    mut requests: mpsc::Receiver<Request>,
    completions: mpsc::Sender<Completion>,
    ctx: UiSignal,
    desktop: CurrentDesktop,
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
                        complete_focus(&completions, id, result);
                    }
                    Request::FocusCurrent { id, snapshot, run } => {
                        let target = current_target(&desktop.view.borrow(), &snapshot, &run);
                        let result = match target {
                            Some(controller) => desktop_focus::request(controller, run.clone()).await,
                            None => Err(anyhow::anyhow!("The displayed agent changed. Review the slots and try again.")),
                        };
                        // An older Desktop, or the window that explicitly
                        // opened Deck when another window owns the endpoint,
                        // can still receive the exact request on this connection.
                        if result.as_ref().is_err_and(|error| {
                            error.is::<EndpointUnavailable>()
                                || error.downcast_ref::<ProtocolFault>().is_some_and(|fault| {
                                    matches!(fault.code.as_str(), "WriterRequired" | "WorkspaceBusy")
                                })
                        })
                            && desktop.legacy.focus(id, run)
                        {
                            continue;
                        }
                        complete_focus(&completions, id, result);
                    }
                }
                ctx.notify();
            }
        }
    }
}

fn complete_focus(completions: &mpsc::Sender<Completion>, id: Uuid, result: Result<()>) {
    let (message, failed) = match result {
        Ok(()) => ("Focused in Corvid Desktop.".into(), false),
        Err(error) => (error.to_string(), true),
    };
    let _ = completions.try_send(Completion {
        id,
        message,
        failed,
        paired: None,
    });
}

fn current_target(display: &Display, shown: &Snapshot, run: &PresentedRunKey) -> Option<Uuid> {
    let latest = display.snapshot.as_ref()?;
    if display.link != Link::Online
        || latest.link != shown.link
        || latest.page != shown.page
        || latest.generation() != shown.generation()
    {
        return None;
    }
    let controller = shown.desktop_run_target(run)?;
    if latest.desktop_run_target(run)? != controller {
        return None;
    }
    // Desktop validates the captured run in the matching epoch. Slot commands
    // still use revisions, but a run can move while an exact focus is in flight.
    Some(controller)
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
    desktop_focus::request(controller, run).await
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
    let mut previous = None;
    loop {
        let slots = get_page(&mut client, 0).await?;
        anyhow::ensure!(
            slots.controller_id == controller.to_string(),
            "unexpected monitor controller"
        );
        if previous.as_ref() != Some(&slots)
            && let Some(runs) = collect_runs(&mut client, &slots).await?
        {
            let pins: BTreeSet<_> = slots
                .slots
                .iter()
                .filter(|slot| slot.pinned)
                .filter_map(|slot| slot.run.as_ref()?.key.as_ref())
                .filter_map(|key| RunKey::try_from(key).ok())
                .collect();
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
                if host.loaded
                    && host.remote_epoch == epoch
                    && host.online == online
                    && host.runs == runs
                    && host.pins == pins
                {
                    return false;
                }
                let host = Arc::make_mut(host);
                host.loaded = true;
                host.remote_epoch = epoch;
                host.online = online;
                host.runs = runs;
                host.pins = pins;
                true
            }) {
                ctx.notify();
            }
            previous = Some(slots);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn get_page(
    client: &mut PresentationClient,
    overflow_page: u32,
) -> Result<PresentationSlots> {
    let query = GetPresentationSlots {
        count: None,
        overflow_page,
    };
    let body = tokio::time::timeout(
        corvid_protocol::COMMAND_TIMEOUT,
        client.request(Body::GetPresentationSlots(query)),
    )
    .await??;
    let Body::PresentationSlots(slots) = body else {
        anyhow::bail!("unexpected slot response");
    };
    Ok(slots)
}

async fn collect_runs(
    client: &mut PresentationClient,
    first: &PresentationSlots,
) -> Result<Option<Vec<PresentedRun>>> {
    anyhow::ensure!(
        first.overflow_count as usize <= MAX_TRACKED
            && first.slots.len() <= corvid_companion::MAX_SLOTS as usize,
        "monitor returned too many runs"
    );
    let mut runs: Vec<_> = first
        .slots
        .iter()
        .filter_map(|slot| slot.run.clone())
        .collect();
    let assigned = runs.len();
    runs.extend(first.overflow.iter().cloned());
    for index in 1..first.overflow_count.div_ceil(PRESENTATION_PAGE_SIZE) {
        let page = get_page(client, index).await?;
        if page.controller_id != first.controller_id
            || page.remote_epoch != first.remote_epoch
            || page.control_generation != first.control_generation
            || page.remote_sequence != first.remote_sequence
            || page.overflow_count != first.overflow_count
            || page.slots != first.slots
        {
            // Retry on the next poll rather than publish a mixed queue.
            return Ok(None);
        }
        runs.extend(page.overflow);
    }
    anyhow::ensure!(
        runs.len() == assigned + first.overflow_count as usize,
        "incomplete monitor queue"
    );
    Ok(Some(runs))
}
