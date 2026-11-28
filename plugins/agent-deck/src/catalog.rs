use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_async::{mpsc, watch};
use corvid_companion::{
    MAX_TRACKED, PRESENTATION_ATTENTION_CAPABILITY, PRESENTATION_RUNS_CAPABILITY,
    PRESENTATION_WATCH_CAPABILITY, PresentationAddress, PresentationClient, RunKey,
};
use corvid_presentation::desktop_focus::{self, EndpointUnavailable};
use corvid_presentation::ipc::{Completion, Link, execute_presentation_command, failed_link};
use corvid_presentation::model::Snapshot;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    CompanionConnection, GetPresentationAttention, GetPresentationRuns, PresentationAttention,
    PresentationRuns, PresentedRun, PresentedRunKey, WaitPresentationChange,
};
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault};
use uuid::Uuid;

use crate::background::Update;
use crate::hosts::{self, Profile};
use crate::parent::DesktopLink;
use crate::ui_signal::UiSignal;

pub(crate) const HOST_PREVIEW_LIMIT: usize = 64;
const DISCOVER_INTERVAL: Duration = Duration::from_secs(2);
const WATCH_WAIT_TIMEOUT: Duration = Duration::from_secs(20);
const MULTI_PAGE_RETRIES: usize = 5;

pub(crate) struct Catalog {
    pub current_host: Uuid,
    pub views: watch::Receiver<View>,
    controls: mpsc::Sender<Control>,
    completed: RefCell<mpsc::Receiver<Completion>>,
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

    pub fn needs_reconnect(&self) -> bool {
        if self.link == Link::Unavailable {
            return true;
        }
        self.link == Link::Online
            && self.pages.first().is_some_and(|snapshot| {
                matches!(
                    CompanionConnection::try_from(snapshot.data.connection),
                    Ok(CompanionConnection::Offline
                        | CompanionConnection::Error
                        | CompanionConnection::Connecting)
                )
            })
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

enum Control {
    Action(Request),
    RefreshHosts,
    ReconnectHost(Uuid),
}

impl Catalog {
    pub fn start(
        state: PathBuf,
        current: Uuid,
        address: <PresentationAddress as ToOwned>::Owned,
        ctx: UiSignal,
        legacy: DesktopLink,
        updates: mpsc::Sender<Update>,
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
        let (controls, incoming) = mpsc::channel(16);
        let (completions, completed) = mpsc::channel(8);
        corvid_async::task::worker("corvid-agent-deck-hosts", move |runtime| {
            runtime.block_on(run(
                (state, current, address),
                profiles,
                publish,
                incoming,
                completions,
                updates,
                ctx,
                legacy,
            ));
        })?;
        Ok(Self {
            current_host: current,
            views,
            controls,
            completed: RefCell::new(completed),
        })
    }

    pub fn try_completion(&self) -> Option<Completion> {
        self.completed.borrow_mut().try_recv().ok()
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

    pub fn refresh_hosts(&self) {
        let _ = self.controls.try_send(Control::RefreshHosts);
    }

    pub fn reconnect_host(&self, host: Uuid) {
        let _ = self.controls.try_send(Control::ReconnectHost(host));
    }

    fn enqueue(&self, id: Uuid, snapshot: Arc<Snapshot>, action: Action) -> bool {
        self.controls
            .try_send(Control::Action(Request {
                id,
                snapshot,
                action,
            }))
            .is_ok()
    }
}

struct MonitorEntry {
    profile: Profile,
    wake: watch::Sender<()>,
    task: corvid_async::task::JoinHandle<()>,
}

#[derive(Default)]
struct Monitors {
    entries: BTreeMap<Uuid, MonitorEntry>,
}

impl Monitors {
    fn refresh(
        &mut self,
        profiles: &BTreeMap<Uuid, Profile>,
        publish: &watch::Sender<View>,
        updates: &mpsc::Sender<Update>,
        ctx: &UiSignal,
    ) {
        self.entries.retain(|id, entry| {
            let keep = profiles.get(id) == Some(&entry.profile) && !entry.task.is_finished();
            if !keep {
                entry.task.abort();
            }
            keep
        });
        for (&id, profile) in profiles {
            self.entries.entry(id).or_insert_with(|| {
                let (wake, wake_rx) = watch::channel(());
                let task = corvid_async::task::spawn(monitor(
                    id,
                    profile.clone(),
                    publish.clone(),
                    updates.clone(),
                    ctx.clone(),
                    wake_rx,
                ));
                MonitorEntry {
                    profile: profile.clone(),
                    wake,
                    task,
                }
            });
        }
    }

    fn wake(&self, id: Uuid) {
        if let Some(entry) = self.entries.get(&id) {
            entry.wake.send_replace(());
        }
    }
}

impl Drop for Monitors {
    fn drop(&mut self) {
        for entry in self.entries.values() {
            entry.task.abort();
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    (state, current, address): (PathBuf, Uuid, <PresentationAddress as ToOwned>::Owned),
    mut profiles: BTreeMap<Uuid, Profile>,
    publish: watch::Sender<View>,
    mut incoming: mpsc::Receiver<Control>,
    completions: mpsc::Sender<Completion>,
    updates: mpsc::Sender<Update>,
    ctx: UiSignal,
    legacy: DesktopLink,
) {
    let mut monitors = Monitors::default();
    let _ = updates.send(Update::Hosts(host_labels(&profiles))).await;
    monitors.refresh(&profiles, &publish, &updates, &ctx);

    let mut refresh = corvid_async::time::interval(DISCOVER_INTERVAL);
    let mut reconnects = corvid_async::task::JoinSet::new();
    loop {
        corvid_async::select! {
            _ = publish.closed() => break,
            _ = reconnects.join_next() if !reconnects.is_empty() => {}
            _ = refresh.tick() => {
                rediscover(
                    &state,
                    current,
                    &address,
                    &mut profiles,
                    &publish,
                    &updates,
                    &mut monitors,
                    &ctx,
                )
                .await;
            }
            control = incoming.recv() => {
                let Some(control) = control else { break };
                match control {
                    Control::Action(request) => {
                        let host_id = request.snapshot.host;
                        let wake_monitor = matches!(request.action, Action::Companion(_));
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
                        if wake_monitor {
                            monitors.wake(host_id);
                        }
                    }
                    Control::RefreshHosts => {
                        rediscover(
                            &state,
                            current,
                            &address,
                            &mut profiles,
                            &publish,
                            &updates,
                            &mut monitors,
                            &ctx,
                        )
                        .await;
                        let view = publish.borrow();
                        for (&id, profile) in &profiles {
                            let needs = view
                                .hosts
                                .get(&id)
                                .is_some_and(|host| host.needs_reconnect());
                            if needs {
                                let addr = profile.address.clone();
                                let wake = monitors.entries.get(&id).map(|e| e.wake.clone());
                                reconnects.spawn(async move {
                                    let _ = trigger_reconnect(&addr).await;
                                    if let Some(wake) = wake {
                                        wake.send_replace(());
                                    }
                                });
                            } else {
                                monitors.wake(id);
                            }
                        }
                    }
                    Control::ReconnectHost(id) => {
                        if let Some(profile) = profiles.get(&id) {
                            let addr = profile.address.clone();
                            let wake = monitors.entries.get(&id).map(|e| e.wake.clone());
                            reconnects.spawn(async move {
                                let _ = trigger_reconnect(&addr).await;
                                if let Some(wake) = wake {
                                    wake.send_replace(());
                                }
                            });
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn rediscover(
    state: &Path,
    current: Uuid,
    address: &<PresentationAddress as ToOwned>::Owned,
    profiles: &mut BTreeMap<Uuid, Profile>,
    publish: &watch::Sender<View>,
    updates: &mpsc::Sender<Update>,
    monitors: &mut Monitors,
    ctx: &UiSignal,
) {
    match hosts::discover_all(state, current) {
        Ok(mut next) => {
            if let Some(profile) = next.get_mut(&current) {
                profile.address = address.clone();
            }
            if next != *profiles {
                publish.send_modify(|view| {
                    view.hosts.retain(|id, _| next.contains_key(id));
                    for (&id, profile) in &next {
                        if profiles.get(&id) != Some(profile) {
                            view.hosts.insert(id, Arc::new(Host::new(profile)));
                        }
                    }
                });
                *profiles = next;
                let _ = updates.send(Update::Hosts(host_labels(profiles))).await;
                ctx.notify();
            }
            monitors.refresh(profiles, publish, updates, ctx);
        }
        Err(error) => {
            let _ = updates
                .send(Update::Snapshot(current, Err(error.to_string())))
                .await;
        }
    }
}

fn host_labels(profiles: &BTreeMap<Uuid, Profile>) -> BTreeMap<Uuid, String> {
    profiles
        .iter()
        .map(|(&id, profile)| (id, profile.label.clone()))
        .collect()
}

async fn trigger_reconnect(address: &PresentationAddress) -> Result<()> {
    let mut client =
        corvid_async::time::timeout(COMMAND_TIMEOUT, PresentationClient::connect(address))
            .await??;
    corvid_async::time::timeout(COMMAND_TIMEOUT, client.reconnect_monitor()).await??;
    Ok(())
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
                let result = request_focus(controller, run.clone()).await;
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

async fn request_focus(controller: Uuid, run: PresentedRunKey) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        desktop_focus::request_with_activation(controller, run, crate::platform::yield_activation)
            .await
    }
    #[cfg(not(target_os = "macos"))]
    desktop_focus::request(controller, run).await
}

fn allows_focus_fallback(error: &anyhow::Error) -> bool {
    error.is::<EndpointUnavailable>()
        || error
            .downcast_ref::<ProtocolFault>()
            .is_some_and(|fault| matches!(fault.code.as_str(), "WriterRequired" | "WorkspaceBusy"))
}

enum SyncOutcome {
    Synced {
        pages: Vec<Arc<Snapshot>>,
        attention: PresentationAttention,
        state_revision: u64,
    },
    Retry,
}

async fn monitor(
    id: Uuid,
    profile: Profile,
    publish: watch::Sender<View>,
    updates: mpsc::Sender<Update>,
    ctx: UiSignal,
    mut wake: watch::Receiver<()>,
) {
    let mut client: Option<PresentationClient> = None;
    let mut failures: u32 = 0;
    loop {
        let step: Result<SyncOutcome> = async {
            let conn = match client.as_mut() {
                Some(conn) => conn,
                None => {
                    let conn = corvid_async::time::timeout(
                        COMMAND_TIMEOUT,
                        PresentationClient::connect(&profile.address),
                    )
                    .await??;
                    ensure!(
                        conn.session().host_id == id.to_string(),
                        "unexpected monitor host"
                    );
                    for required in [
                        PRESENTATION_RUNS_CAPABILITY,
                        PRESENTATION_ATTENTION_CAPABILITY,
                        PRESENTATION_WATCH_CAPABILITY,
                    ] {
                        ensure!(
                            conn.session()
                                .capabilities
                                .iter()
                                .any(|cap| cap == required),
                            ProtocolFault::new(
                                "UnsupportedCapability",
                                "current agent lists require a newer companion"
                            ),
                        );
                    }
                    client.insert(conn)
                }
            };
            corvid_async::time::timeout(COMMAND_TIMEOUT, sync_once(conn, &profile)).await?
        }
        .await;

        match step {
            Ok(SyncOutcome::Synced {
                pages,
                attention,
                state_revision,
            }) => {
                failures = 0;
                if publish.send_if_modified(|view| {
                    let Some(host) = view.hosts.get_mut(&id) else {
                        return false;
                    };
                    if host.link == Link::Online && host.pages == pages {
                        return false;
                    }
                    let host = Arc::make_mut(host);
                    host.link = Link::Online;
                    host.pages = pages;
                    true
                }) {
                    ctx.notify();
                }
                if updates
                    .send(Update::Snapshot(id, Ok(attention)))
                    .await
                    .is_err()
                {
                    return;
                }
                let Some(conn) = client.as_mut() else {
                    continue;
                };
                let wait = corvid_async::time::timeout(
                    WATCH_WAIT_TIMEOUT,
                    conn.request(Body::WaitPresentationChange(WaitPresentationChange {
                        after_revision: state_revision,
                    })),
                );
                corvid_async::select! {
                    changed = wake.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        let _ = wake.borrow_and_update();
                        // Cancelling an in-flight PresentationClient::request leaves it unusable.
                        client = None;
                    }
                    result = wait => {
                        match result {
                            Ok(Ok(Body::PresentationChanged(_))) => {}
                            Ok(Ok(_)) => {
                                client = None;
                            }
                            Ok(Err(_)) | Err(_) => {
                                client = None;
                            }
                        }
                    }
                }
            }
            Ok(SyncOutcome::Retry) => {
                corvid_async::select! {
                    changed = wake.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        let _ = wake.borrow_and_update();
                    }
                    _ = corvid_async::time::sleep(Duration::from_millis(20)) => {}
                }
            }
            Err(error) => {
                client = None;
                let link = failed_link(&error);
                if publish.send_if_modified(|view| {
                    let Some(host) = view.hosts.get_mut(&id) else {
                        return false;
                    };
                    if host.link == link && host.pages.is_empty() {
                        return false;
                    }
                    let host = Arc::make_mut(host);
                    host.link = link;
                    host.pages = Vec::new();
                    true
                }) {
                    ctx.notify();
                }
                if updates
                    .send(Update::Snapshot(id, Err(error.to_string())))
                    .await
                    .is_err()
                {
                    return;
                }
                let delay =
                    Duration::from_secs(1u64 << failures.min(2)).min(Duration::from_secs(3));
                failures = failures.saturating_add(1);
                corvid_async::select! {
                    changed = wake.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        let _ = wake.borrow_and_update();
                        failures = 0;
                    }
                    _ = corvid_async::time::sleep(delay) => {}
                }
            }
        }
    }
}

async fn sync_once(client: &mut PresentationClient, profile: &Profile) -> Result<SyncOutcome> {
    let link = Uuid::parse_str(&client.session().server_epoch)?;
    let mut pages = None;
    let mut state_revision = 0;
    for attempt in 0..MULTI_PAGE_RETRIES {
        let (first, revision, state_rev) = get_page(client, profile.controller, link, 0).await?;
        ensure!(
            first.data.overflow_count as usize <= MAX_TRACKED
                && first.data.suppressed_count as usize <= MAX_TRACKED,
            "monitor returned too many runs",
        );
        let first = Arc::new(first);
        let mut collected = vec![first.clone()];
        let mut conflicted = false;
        for index in 1..first.page_count() {
            let (page, version, page_state_rev) =
                get_page(client, profile.controller, link, index).await?;
            if version != revision || !matches_snapshot(&first, &page) {
                conflicted = true;
                break;
            }
            state_revision = state_revision.max(page_state_rev);
            collected.push(Arc::new(page));
        }
        if !conflicted {
            validate_pages(&collected)?;
            state_revision = state_revision.max(state_rev);
            pages = Some(collected);
            break;
        }
        if attempt + 1 < MULTI_PAGE_RETRIES {
            corvid_async::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let Some(pages) = pages else {
        return Ok(SyncOutcome::Retry);
    };

    let reply = client
        .request(Body::GetPresentationAttention(GetPresentationAttention {}))
        .await?;
    let Body::PresentationAttention(attention) = reply else {
        anyhow::bail!("unexpected notification reply");
    };
    ensure!(
        attention.controller_id == profile.controller.to_string(),
        "unexpected companion controller"
    );
    Ok(SyncOutcome::Synced {
        pages,
        attention,
        state_revision,
    })
}

async fn get_page(
    client: &mut PresentationClient,
    controller: Uuid,
    link: Uuid,
    page: u32,
) -> Result<(Snapshot, u64, u64)> {
    let query = Body::GetPresentationRuns(GetPresentationRuns { page });
    let Body::PresentationRuns(PresentationRuns {
        page: data,
        layout_revision,
        state_revision,
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
    Ok((snapshot, layout_revision, state_revision))
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
