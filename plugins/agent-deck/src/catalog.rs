use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_async::{Semaphore, mpsc, watch};
use corvid_companion::{
    MAX_TRACKED, PRESENTATION_RUNS_CAPABILITY, PresentationAddress, PresentationClient, RunKey,
};
use corvid_presentation::desktop_focus::{self, EndpointUnavailable};
use corvid_presentation::ipc::{Completion, Link, execute_presentation_command, failed_link};
use corvid_presentation::model::Snapshot;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{GetPresentationRuns, PresentationRuns, PresentedRun, PresentedRunKey};
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault};
use uuid::Uuid;

use crate::hosts::{self, Profile, Subscriptions};
use crate::parent::DesktopLink;
use crate::ui_signal::UiSignal;

pub(crate) const HOST_PREVIEW_LIMIT: usize = 64;
const REFRESH: Duration = Duration::from_secs(1);
const MAX_CONCURRENT_POLLS: usize = 8;

pub(crate) struct Catalog {
    pub views: watch::Receiver<View>,
    requests: mpsc::Sender<Request>,
    pub completed: mpsc::Receiver<Completion>,
}

#[derive(Clone, Default)]
pub(crate) struct View {
    pub hosts: BTreeMap<Uuid, Arc<Host>>,
}

#[derive(Clone, PartialEq)]
pub(crate) struct Host {
    pub label: String,
    pub link: Link,
    pub pages: Vec<Arc<Snapshot>>,
}

impl Host {
    fn new(profile: &Profile) -> Self {
        Self {
            label: profile.label.clone(),
            link: Link::Connecting,
            pages: Vec::new(),
        }
    }

    pub fn accepts(&self, shown: &Snapshot) -> bool {
        self.link == Link::Online
            && shown.generation().is_some()
            && self.pages.first().is_some_and(|latest| {
                latest.link == shown.link && latest.generation() == shown.generation()
            })
    }

    pub fn runs(&self) -> impl Iterator<Item = (&Arc<Snapshot>, &PresentedRun)> {
        self.pages
            .first()
            .into_iter()
            .flat_map(|snapshot| {
                snapshot
                    .data
                    .slots
                    .iter()
                    .filter_map(move |slot| Some((snapshot, slot.run.as_ref()?)))
            })
            .chain(self.pages.iter().flat_map(|snapshot| {
                snapshot
                    .data
                    .overflow
                    .iter()
                    .map(move |run| (snapshot, run))
            }))
    }

    pub fn focus_snapshot(&self, key: &PresentedRunKey) -> Option<&Arc<Snapshot>> {
        self.runs()
            .find(|(snapshot, run)| snapshot.live_key(run) == Some(key))
            .map(|(snapshot, _)| snapshot)
    }
}

enum Action {
    Focus {
        run: PresentedRunKey,
        require_card: bool,
    },
    Companion(Box<Body>),
}

struct Request {
    id: Uuid,
    snapshot: Arc<Snapshot>,
    action: Action,
}

impl Catalog {
    pub fn start(
        state: PathBuf,
        current: Uuid,
        address: <PresentationAddress as ToOwned>::Owned,
        ctx: UiSignal,
        legacy: DesktopLink,
    ) -> Result<Self> {
        let mut profiles = hosts::discover_all(&state, current)?;
        if let Some(profile) = profiles.get_mut(&current) {
            profile.address = address.clone();
        }
        let hosts = profiles
            .iter()
            .map(|(&id, profile)| (id, Arc::new(Host::new(profile))))
            .collect();
        let (publish, views) = watch::channel(View { hosts });
        let (requests, incoming) = mpsc::channel(4);
        let (completions, completed) = mpsc::channel(4);
        corvid_async::task::worker("corvid-agent-deck-hosts", move |runtime| {
            runtime.block_on(run(
                (state, current, address),
                profiles,
                publish,
                incoming,
                completions,
                ctx,
                legacy,
            ));
        })?;
        Ok(Self {
            views,
            requests,
            completed,
        })
    }

    pub fn focus(
        &self,
        id: Uuid,
        snapshot: Arc<Snapshot>,
        run: PresentedRunKey,
        require_card: bool,
    ) -> bool {
        self.enqueue(id, snapshot, Action::Focus { run, require_card })
    }

    pub fn submit(&self, id: Uuid, snapshot: Arc<Snapshot>, body: Body) -> bool {
        self.enqueue(id, snapshot, Action::Companion(Box::new(body)))
    }

    fn enqueue(&self, id: Uuid, snapshot: Arc<Snapshot>, action: Action) -> bool {
        self.requests
            .try_send(Request {
                id,
                snapshot,
                action,
            })
            .is_ok()
    }
}

async fn run(
    (state, current, address): (PathBuf, Uuid, <PresentationAddress as ToOwned>::Owned),
    mut profiles: BTreeMap<Uuid, Profile>,
    publish: watch::Sender<View>,
    mut requests: mpsc::Receiver<Request>,
    completions: mpsc::Sender<Completion>,
    ctx: UiSignal,
    legacy: DesktopLink,
) {
    let mut subscriptions = Subscriptions::default();
    let mut refresh = corvid_async::time::interval(REFRESH);
    let polls = Arc::new(Semaphore::new(MAX_CONCURRENT_POLLS));
    loop {
        corvid_async::select! {
            _ = publish.closed() => break,
            _ = refresh.tick() => {
                if let Ok(mut next) = hosts::discover_all(&state, current) {
                    if let Some(profile) = next.get_mut(&current) {
                        profile.address = address.clone();
                    }
                    if next != profiles {
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
                }
                subscriptions.refresh(&profiles, |id, profile| {
                    monitor(id, profile, publish.clone(), ctx.clone(), polls.clone())
                });
            }
            request = requests.recv() => {
                let Some(request) = request else { break };
                let id = request.id;
                if let Some(result) = execute(request, &profiles, &publish, current, &legacy).await {
                    let completion = result.unwrap_or_else(|error| Completion {
                        id,
                        message: error.to_string(),
                        failed: true,
                        paired: None,
                    });
                    let _ = completions.try_send(completion);
                    ctx.notify();
                }
            }
        }
    }
}

async fn execute(
    request: Request,
    profiles: &BTreeMap<Uuid, Profile>,
    publish: &watch::Sender<View>,
    current: Uuid,
    legacy: &DesktopLink,
) -> Option<Result<Completion>> {
    let Request {
        id,
        snapshot,
        action,
    } = request;
    let host = publish.borrow().hosts.get(&snapshot.host).cloned();
    let result = async {
        let host = host
            .filter(|host| host.accepts(&snapshot))
            .context("The host connection changed. Review the updated agents and try again.")?;
        match action {
            Action::Focus { run, require_card } => {
                ensure!(
                    !require_card || host.focus_snapshot(&run).is_some(),
                    "The agent is no longer available."
                );
                let controller = snapshot
                    .desktop_run_target(&run)
                    .context("Invalid focus target")?;
                let result = desktop_focus::request(controller, run.clone()).await;
                if snapshot.host == current
                    && result.as_ref().is_err_and(allows_focus_fallback)
                    && legacy.focus(id, run)
                {
                    return Ok(None);
                }
                result?;
                Ok(Some(Completion {
                    id,
                    message: "Focused in Corvid Desktop.".into(),
                    failed: false,
                    paired: None,
                }))
            }
            Action::Companion(body) => {
                let profile = profiles
                    .get(&snapshot.host)
                    .context("The host was removed.")?;
                let mut client = PresentationClient::connect(&profile.address).await?;
                ensure!(
                    client.session().host_id == snapshot.host.to_string()
                        && client.session().server_epoch == snapshot.link.to_string(),
                    "The companion changed. Review the updated agents and try again.",
                );
                execute_presentation_command(&mut client, id, *body)
                    .await
                    .map(Some)
            }
        }
    }
    .await;
    result.transpose()
}

fn allows_focus_fallback(error: &anyhow::Error) -> bool {
    error.is::<EndpointUnavailable>()
        || error
            .downcast_ref::<ProtocolFault>()
            .is_some_and(|fault| matches!(fault.code.as_str(), "WriterRequired" | "WorkspaceBusy"))
}

async fn monitor(
    id: Uuid,
    profile: Profile,
    publish: watch::Sender<View>,
    ctx: UiSignal,
    polls: Arc<Semaphore>,
) {
    loop {
        let result = {
            let _permit = polls.acquire().await;
            corvid_async::time::timeout(COMMAND_TIMEOUT, poll_host(id, &profile)).await
        };
        let (link, pages) = match result {
            Ok(Ok(Some(pages))) => (Link::Online, pages),
            Ok(Ok(None)) => (Link::Connecting, Vec::new()),
            Ok(Err(error)) => (failed_link(&error), Vec::new()),
            Err(_) => (Link::Unavailable, Vec::new()),
        };
        if publish.send_if_modified(|view| {
            let Some(host) = view.hosts.get_mut(&id) else {
                return false;
            };
            if host.link == link && host.pages == pages {
                return false;
            }
            let host = Arc::make_mut(host);
            host.link = link;
            host.pages = pages;
            true
        }) {
            ctx.notify();
        }
        corvid_async::time::sleep(REFRESH).await;
    }
}

async fn poll_host(id: Uuid, profile: &Profile) -> Result<Option<Vec<Arc<Snapshot>>>> {
    let mut client = PresentationClient::connect(&profile.address).await?;
    ensure!(
        client.session().host_id == id.to_string(),
        "unexpected monitor host"
    );
    ensure!(
        client
            .session()
            .capabilities
            .iter()
            .any(|cap| cap == PRESENTATION_RUNS_CAPABILITY),
        ProtocolFault::new(
            "UnsupportedCapability",
            "current agent lists require a newer companion"
        ),
    );
    let link = Uuid::parse_str(&client.session().server_epoch)?;
    let (first, revision) = get_page(&mut client, profile.controller, link, 0).await?;
    ensure!(
        first.data.overflow_count as usize <= MAX_TRACKED
            && first.data.suppressed_count as usize <= MAX_TRACKED,
        "monitor returned too many runs",
    );
    let first = Arc::new(first);
    let mut pages = vec![first.clone()];
    for index in 1..first.page_count() {
        let (page, version) = get_page(&mut client, profile.controller, link, index).await?;
        if version != revision || !matches_snapshot(&first, &page) {
            return Ok(None);
        }
        pages.push(Arc::new(page));
    }
    validate_pages(&pages)?;
    Ok(Some(pages))
}

async fn get_page(
    client: &mut PresentationClient,
    controller: Uuid,
    link: Uuid,
    page: u32,
) -> Result<(Snapshot, u64)> {
    let query = Body::GetPresentationRuns(GetPresentationRuns { page });
    let Body::PresentationRuns(PresentationRuns {
        page: data,
        layout_revision,
    }) = client.request(query).await?
    else {
        anyhow::bail!("unexpected current agent list response");
    };
    let data = data.context("missing current agent list")?;
    ensure!(
        layout_revision > 0 && data.controller_id == controller.to_string(),
        "invalid current agent list identity",
    );
    let snapshot = Snapshot::new(link, client.session(), page, data)?;
    ensure!(
        (snapshot.generation().is_some()
            && snapshot
                .data
                .remote_epoch
                .as_deref()
                .and_then(|epoch| Uuid::parse_str(epoch).ok())
                .is_some()
            && snapshot.data.remote_sequence.is_some())
            || (snapshot.data.slots.iter().all(|slot| slot.run.is_none())
                && snapshot.data.overflow_count == 0
                && snapshot.data.suppressed_count == 0),
        "disconnected host returned current agent runs",
    );
    Ok((snapshot, layout_revision))
}

fn matches_snapshot(first: &Snapshot, page: &Snapshot) -> bool {
    let first = &first.data;
    let page = &page.data;
    first.connection == page.connection
        && first.remote_epoch == page.remote_epoch
        && first.remote_sequence == page.remote_sequence
        && first.control_generation == page.control_generation
        && first.overflow_count == page.overflow_count
        && first.suppressed_count == page.suppressed_count
        && first.slots == page.slots
        && first.bindings == page.bindings
}

fn validate_pages(pages: &[Arc<Snapshot>]) -> Result<()> {
    let first = &pages[0];
    ensure!(
        pages
            .iter()
            .map(|page| page.data.overflow.len())
            .sum::<usize>()
            == first.data.overflow_count as usize
            && pages
                .iter()
                .map(|page| page.data.suppressed.len())
                .sum::<usize>()
                == first.data.suppressed_count as usize,
        "incomplete current agent list",
    );
    let mut seen = BTreeSet::new();
    for key in first
        .data
        .slots
        .iter()
        .filter_map(|slot| slot.run.as_ref())
        .chain(pages.iter().flat_map(|page| &page.data.overflow))
        .map(|run| run.key.as_ref())
        .chain(
            pages
                .iter()
                .flat_map(|page| page.data.suppressed.iter().map(Some)),
        )
    {
        let key = key.context("missing current agent run key")?;
        let parsed = RunKey::try_from(key)?;
        ensure!(
            parsed.host == first.host
                && first.data.remote_epoch.as_deref() == Some(&key.remote_epoch)
                && seen.insert(parsed),
            "unexpected current agent run key",
        );
    }
    Ok(())
}
