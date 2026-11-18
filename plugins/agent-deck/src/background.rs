use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::ui_signal::UiSignal;
use anyhow::{Context, Result, ensure};
use corvid_agent_deck::connection;
use corvid_companion::{
    Config, PRESENTATION_ATTENTION_CAPABILITY, PresentationAddress, PresentationClient,
};
use corvid_presentation::{desktop_focus, ipc::Completion};
use corvid_protocol::COMMAND_TIMEOUT;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{GetPresentationAttention, PresentationAttention};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use crate::delivery::Delivery;
use crate::hosts::{self, Subscriptions};
use crate::parent::{self, DesktopLink};
use crate::platform::Callbacks;
use corvid_agent_deck_core::native::{
    Availability, Capabilities, Event, NativeError, NativeServices, Status,
};
use corvid_agent_deck_core::notifications::{Ledger, Notice, Settings, now_ms};

#[cfg(test)]
mod tests;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use corvid_protocol::unix::RuntimeDir as StateDirectory;
#[cfg(windows)]
use corvid_protocol::windows::StateDirectory;

#[derive(Clone, Default)]
pub struct View {
    pub settings: Settings,
    pub history: Vec<Notice>,
    pub selected: Option<Notice>,
    pub status: String,
    pub capabilities: Capabilities,
    pub notifications_open: u64,
}

pub struct Background {
    pub desktop: DesktopLink,
    completed: RefCell<mpsc::Receiver<Completion>>,
    pub view: watch::Receiver<View>,
    settings: watch::Sender<Settings>,
    test: mpsc::Sender<()>,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl Background {
    pub fn start<N: NativeServices>(
        state: PathBuf,
        address: <PresentationAddress as ToOwned>::Owned,
        config: Config,
        ctx: UiSignal,
        native: impl FnOnce(Callbacks, Uuid, &Path, &PresentationAddress) -> Result<N> + Send + 'static,
    ) -> Result<Self> {
        let desktop = DesktopLink::default();
        let (completed, completions) = mpsc::channel(8);
        let (views, view) = watch::channel(View::default());
        let (settings, changed) = watch::channel(Settings::default());
        let (test, tests) = mpsc::channel(1);
        let (stop, stopped) = oneshot::channel();
        let (started, ready) = std::sync::mpsc::sync_channel(1);
        let bridge = desktop.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let worker = std::thread::Builder::new()
            .name("corvid-agent-deck-background".into())
            .spawn(move || {
                runtime.block_on(async move {
                    let channels = Channels {
                        bridge,
                        completed,
                        views: views.clone(),
                        settings: changed,
                        tests,
                        ctx: ctx.clone(),
                        started: started.clone(),
                        stopped,
                    };
                    if let Err(error) = run(state, address, config, channels, native).await {
                        let _ = started.try_send(Err(error.to_string()));
                        views.send_modify(|view| {
                            view.status = error.to_string();
                            view.capabilities = view.capabilities.stopped();
                        });
                        parent::show(&ctx);
                    }
                });
            })?;
        let result = ready
            .recv_timeout(Duration::from_secs(10))
            .context("Agent Deck background startup timed out")
            .and_then(|result| result.map_err(anyhow::Error::msg));
        if let Err(error) = result {
            let _ = stop.send(());
            let _ = worker.join();
            return Err(error);
        }
        Ok(Self {
            desktop,
            completed: RefCell::new(completions),
            view,
            settings,
            test,
            stop: Some(stop),
            worker: Some(worker),
        })
    }

    pub fn try_completion(&self) -> Option<Completion> {
        self.completed.borrow_mut().try_recv().ok()
    }

    pub fn configure(&self, settings: Settings) {
        self.settings.send_replace(settings);
    }

    pub fn test_notification(&self) {
        let _ = self.test.try_send(());
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Channels {
    bridge: DesktopLink,
    completed: mpsc::Sender<Completion>,
    views: watch::Sender<View>,
    settings: watch::Receiver<Settings>,
    tests: mpsc::Receiver<()>,
    ctx: UiSignal,
    started: std::sync::mpsc::SyncSender<Result<(), String>>,
    stopped: oneshot::Receiver<()>,
}

async fn run<N: NativeServices>(
    state: PathBuf,
    address: <PresentationAddress as ToOwned>::Owned,
    config: Config,
    channels: Channels,
    start_native: impl FnOnce(Callbacks, Uuid, &Path, &PresentationAddress) -> Result<N>,
) -> Result<()> {
    let Channels {
        bridge,
        completed,
        views,
        mut settings,
        mut tests,
        ctx,
        started,
        mut stopped,
    } = channels;
    let storage = StateDirectory::create_all(&state.join("agent-deck"))?;
    let _lock = storage.lock_storage()?;
    let ledger: Ledger = storage
        .read_private_file("notifications.json", 1024 * 1024)?
        .map(|bytes| serde_json::from_slice(&bytes))
        .transpose()?
        .unwrap_or_default();
    ledger.validate()?;
    let (events, mut event_rx) = mpsc::channel(32);
    let (issues, mut issue_rx) = watch::channel(None);
    let (capabilities, mut capability_rx) = watch::channel(Capabilities::default());
    let callbacks = Callbacks {
        events,
        issues,
        capabilities,
        signal: ctx.clone(),
    };
    let (native, issue) = match start_native(callbacks, config.controller_id, &state, &address) {
        Ok(native) => (Some(native), String::new()),
        Err(error) => (None, error.to_string()),
    };
    let mut capabilities = *capability_rx.borrow_and_update();
    if native.is_none() {
        capabilities = capabilities.stopped();
    }
    views.send_replace(View {
        settings: ledger.settings.clone(),
        history: ledger.history.iter().rev().cloned().collect(),
        status: issue.clone(),
        capabilities,
        ..Default::default()
    });
    let (peers, mut peer_rx) = mpsc::channel(8);
    let (listening, listening_rx) = oneshot::channel();
    let mut jobs = tokio::task::JoinSet::new();
    jobs.spawn(connection::listen(config.controller_id, peers, listening));
    tokio::select! {
        _ = &mut stopped => return Ok(()),
        result = listening_rx => result.context("Agent Deck instance already exists or its private endpoint is unavailable")?,
        result = jobs.join_next() => { anyhow::bail!("Agent Deck endpoint unavailable: {result:?}"); }
    }
    let delivery = Delivery::open(hosts::root(&state, config.host_id))?;
    let (snapshots, mut snapshot_rx) = mpsc::channel(8);
    jobs.spawn(poll_hosts(
        state.clone(),
        address,
        config.host_id,
        snapshots,
    ));
    let epoch = Uuid::new_v4();
    let mut clients = tokio::task::JoinSet::new();
    let mut monitor = Monitor {
        storage,
        delivery,
        hosts: BTreeMap::new(),
        ledger,
        native,
        capabilities,
        last_status: None,
        issue,
        views,
        ctx,
    };
    let _ = started.send(Ok(()));
    monitor.restore_login().await;
    let mut capabilities_open = true;
    let mut issues_open = true;
    let mut events_open = true;
    let startup_deadline = sleep(Duration::from_secs(5));
    tokio::pin!(startup_deadline);
    loop {
        tokio::select! {
            // Finish any settings transaction before shutting down the adapter.
            _ = &mut stopped => return Ok(()),
            _ = &mut startup_deadline, if monitor.capabilities != monitor.capabilities.startup_timed_out() => {
                // A backend that never completes initialization cannot leave
                // its window hidden. A late readiness update can still recover.
                monitor.set_capabilities(monitor.capabilities.startup_timed_out());
                monitor.error("Native service initialization timed out".into());
            }
            peer = peer_rx.recv() => {
                let peer = peer.context("Agent Deck listener closed")?;
                if clients.len() < 8 {
                    let bridge = bridge.clone();
                    let scope = parent::Scope {
                        host: config.host_id,
                        controller: config.controller_id,
                        epoch,
                        completed: completed.clone(),
                        ctx: monitor.ctx.clone(),
                    };
                    clients.spawn(async move { bridge.serve(peer, scope).await });
                }
            }
            _ = clients.join_next(), if !clients.is_empty() => {}
            result = jobs.join_next() => {
                anyhow::bail!("Agent Deck background service stopped: {result:?}");
            }
            result = snapshot_rx.recv(), if monitor.capabilities.notifications != Availability::Starting => {
                monitor.snapshot(result.context("notification poller stopped")?).await?;
            }
            result = settings.changed() => {
                result?;
                let next = settings.borrow_and_update().clone();
                monitor.configure(next).await;
            }
            test = tests.recv() => {
                test.context("notification test channel closed")?;
                let activation = monitor.capabilities.activation.ready();
                monitor.notify(test_notice(config.host_id, activation)).await;
            }
            result = capability_rx.changed(), if capabilities_open => {
                capabilities_open = result.is_ok();
                let next = if capabilities_open {
                    *capability_rx.borrow_and_update()
                } else {
                    (*capability_rx.borrow()).stopped()
                };
                if monitor.capabilities != next {
                    let restore = !monitor.capabilities.login_startup.ready()
                        && next.login_startup.ready();
                    monitor.set_capabilities(next);
                    if restore {
                        monitor.restore_login().await;
                    }
                }
            }
            result = issue_rx.changed(), if issues_open => {
                issues_open = result.is_ok();
                if issues_open && let Some(issue) = issue_rx.borrow_and_update().clone() {
                    monitor.error(issue.to_string());
                }
            }
            event = event_rx.recv(), if events_open => {
                match event {
                    Some(event) => monitor.event(event, &state, config.host_id).await,
                    None => events_open = false,
                }
            }
        }
    }
}

struct Monitor<N> {
    storage: StateDirectory,
    delivery: Delivery,
    hosts: BTreeMap<Uuid, HostStatus>,
    ledger: Ledger,
    native: Option<N>,
    capabilities: Capabilities,
    last_status: Option<Status>,
    issue: String,
    views: watch::Sender<View>,
    ctx: UiSignal,
}

#[derive(Default)]
struct HostStatus {
    connected: bool,
    unseen: usize,
    issue: String,
}

enum Update {
    Hosts(Vec<Uuid>),
    Snapshot(Uuid, Result<PresentationAttention, String>),
}

impl<N: NativeServices> Monitor<N> {
    async fn snapshot(&mut self, update: Update) -> Result<()> {
        let (host, result) = match update {
            Update::Hosts(hosts) => {
                self.hosts.retain(|id, _| hosts.contains(id));
                self.publish();
                return Ok(());
            }
            Update::Snapshot(host, result) => (host, result),
        };
        let snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(issue) => {
                self.hosts.insert(
                    host,
                    HostStatus {
                        issue,
                        ..Default::default()
                    },
                );
                self.publish();
                return Ok(());
            }
        };
        let observation = match self.ledger.observe(host, &snapshot, now_ms()) {
            Ok(observation) => observation,
            Err(error) => {
                self.hosts.insert(
                    host,
                    HostStatus {
                        issue: error.to_string(),
                        ..Default::default()
                    },
                );
                self.publish();
                return Ok(());
            }
        };
        let notices = if self.capabilities.notifications.ready() {
            self.delivery.claim(observation.notices).await?
        } else {
            Vec::new()
        };
        if observation.changed {
            save(&self.storage, &self.ledger)?;
        }
        let connected = snapshot.connection
            == corvid_protocol::wire::CompanionConnection::Ready as i32
            && snapshot.control_generation.is_some();
        let issue = if snapshot.omitted_count > 0 {
            format!(
                "{} older notifications exceed the bounded feed.",
                snapshot.omitted_count
            )
        } else if !connected {
            "Waiting for host synchronization".into()
        } else {
            String::new()
        };
        self.hosts.insert(
            host,
            HostStatus {
                connected,
                unseen: snapshot.attention.len(),
                issue,
            },
        );
        self.publish();
        for notice in batch(notices) {
            self.notify(notice).await;
        }
        Ok(())
    }

    fn publish(&mut self) {
        let mut issues = Vec::new();
        if !self.issue.is_empty() {
            issues.push(self.issue.clone());
        }
        for (id, host) in &self.hosts {
            if !host.issue.is_empty() {
                issues.push(format!(
                    "Host {}: {}",
                    &id.simple().to_string()[..8],
                    host.issue
                ));
            }
        }
        self.views.send_modify(|view| {
            view.history = self.ledger.history.iter().rev().cloned().collect();
            view.status = issues.join("; ");
        });
        self.update_status();
        self.ctx.notify();
    }

    fn update_status(&mut self) {
        if !self.capabilities.status_icon.ready() {
            return;
        }
        let Some(native) = &mut self.native else {
            return;
        };
        let online = self.hosts.values().filter(|host| host.connected).count();
        let unseen: usize = self.hosts.values().map(|host| host.unseen).sum();
        let status = Status {
            text: format!("Corvid Agent Deck — {online} connected hosts — {unseen} unseen"),
            attention: unseen > 0,
        };
        if self.last_status.as_ref() != Some(&status) {
            match native.update_status(status.clone()) {
                Ok(()) => self.last_status = Some(status),
                Err(error) => self.error(error.to_string()),
            }
        }
    }

    fn set_capabilities(&mut self, next: Capabilities) {
        let lost_icon = self.capabilities.can_start_hidden() && !next.can_start_hidden();
        if self.capabilities.status_icon != next.status_icon {
            self.last_status = None;
        }
        self.capabilities = next;
        self.views.send_modify(|view| view.capabilities = next);
        self.update_status();
        if lost_icon {
            parent::show(&self.ctx);
        }
        self.ctx.notify();
    }

    async fn notify(&mut self, notice: Notice) {
        let result = match (self.capabilities.notifications.check(), &mut self.native) {
            (Ok(()), Some(native)) => {
                let sound = self.capabilities.sound.ready() && self.ledger.settings.sound;
                native.notify(notice, sound).await
            }
            (Err(error), _) => Err(error),
            _ => Err(NativeError::Unavailable),
        };
        if let Err(error) = result {
            self.error(error.to_string());
        }
    }

    async fn set_login_startup(&mut self, enabled: bool) -> Result<(), NativeError> {
        self.capabilities.login_startup.check()?;
        let native = self.native.as_mut().ok_or(NativeError::Unavailable)?;
        native.set_login_startup(enabled).await
    }

    async fn restore_login(&mut self) {
        if self.ledger.settings.start_at_login
            && self.capabilities.login_startup.ready()
            && let Err(error) = self.set_login_startup(true).await
        {
            self.error(format!("Could not restore login startup: {error}"));
        }
    }

    async fn configure(&mut self, mut next: Settings) {
        if next.sound != self.ledger.settings.sound && !self.capabilities.sound.ready() {
            next.sound = self.ledger.settings.sound;
        }
        let previous_login = self.ledger.settings.start_at_login;
        if next.start_at_login != previous_login
            && let Err(error) = self.set_login_startup(next.start_at_login).await
        {
            next.start_at_login = previous_login;
            self.error(format!("Could not change login startup: {error}"));
        }

        let login_changed = next.start_at_login != previous_login;
        let previous = std::mem::replace(&mut self.ledger.settings, next);
        if let Err(error) = save(&self.storage, &self.ledger) {
            self.ledger.settings = previous;
            let mut issue = format!("Could not save notification settings: {error}");
            if login_changed && let Err(error) = self.set_login_startup(previous_login).await {
                issue.push_str(&format!("; could not restore login startup: {error}"));
            }
            self.error(issue);
        }
        self.views
            .send_modify(|view| view.settings = self.ledger.settings.clone());
        self.ctx.notify();
    }

    fn error(&mut self, issue: String) {
        self.issue = issue;
        self.views
            .send_modify(|view| view.status = self.issue.clone());
        self.ctx.notify();
    }

    async fn event(&mut self, event: Event, state: &Path, current: Uuid) {
        match event {
            Event::OpenNotifications => self.open_notice(None),
            Event::Activate(notice) if notice.summary => self.open_notice(Some(notice)),
            Event::Activate(notice) => {
                let error = focus_notice(state, current, &notice)
                    .await
                    .err()
                    .map(|error| error.to_string());
                if self.ledger.record_focus(notice, error) {
                    if let Err(error) = save(&self.storage, &self.ledger) {
                        self.error(format!("Could not save notification focus result: {error}"));
                    }
                    self.publish();
                }
            }
            Event::TogglePause => {
                let mut settings = self.ledger.settings.clone();
                let now = now_ms();
                settings.paused_until_ms = if settings.paused_until_ms > now {
                    0
                } else {
                    now.saturating_add(60 * 60 * 1000)
                };
                self.configure(settings).await;
            }
        }
    }

    fn open_notice(&self, notice: Option<Notice>) {
        self.views.send_modify(|view| {
            view.notifications_open += 1;
            view.selected = notice;
        });
        parent::show(&self.ctx);
    }
}

async fn focus_notice(state: &Path, current: Uuid, notice: &Notice) -> Result<()> {
    let profiles = hosts::discover(state, current)?;
    let profile = profiles
        .get(&notice.key.host)
        .context("This notification's host is no longer configured.")?;
    let run = notice.key.run();
    #[cfg(target_os = "macos")]
    let result = desktop_focus::request_with_activation(
        profile.controller,
        run,
        crate::platform::yield_activation,
    )
    .await;
    #[cfg(not(target_os = "macos"))]
    let result = desktop_focus::request(profile.controller, run).await;
    result
}

fn batch(mut notices: Vec<Notice>) -> Vec<Notice> {
    if notices.len() <= 3 {
        return notices;
    }
    let count = notices.len();
    let mut summary = notices.pop().unwrap();
    summary.title = format!("{count} new agent notifications");
    summary.body = "Open Corvid Agent Deck to review them.".into();
    summary.summary = true;
    vec![summary]
}

fn test_notice(host: Uuid, activation: bool) -> Notice {
    Notice {
        summary: true,
        key: corvid_agent_deck_core::notifications::Key {
            host,
            epoch: Uuid::new_v4(),
            run: Uuid::new_v4(),
            attention: Uuid::new_v4(),
        },
        title: "Corvid Agent Deck test notification".into(),
        body: if activation {
            "Notifications are working. Click to open notification settings."
        } else {
            "Notifications are working."
        }
        .into(),
        created_ms: now_ms(),
        focus_error: None,
    }
}

fn save(storage: &StateDirectory, ledger: &Ledger) -> Result<()> {
    storage.replace_private_file("notifications.json", &serde_json::to_vec(ledger)?)
}

async fn poll_hosts(
    state: PathBuf,
    address: <PresentationAddress as ToOwned>::Owned,
    current: Uuid,
    snapshots: mpsc::Sender<Update>,
) -> Result<()> {
    let mut subscriptions = Subscriptions::default();
    loop {
        match hosts::discover(&state, current) {
            Ok(mut profiles) => {
                // Preserve an explicitly supplied IPC address for this Deck's scope.
                if let Some(profile) = profiles.get_mut(&current) {
                    profile.address = address.clone();
                }
                snapshots
                    .send(Update::Hosts(profiles.keys().copied().collect()))
                    .await?;
                subscriptions.refresh(&profiles, |id, profile| {
                    poll(profile.address, id, profile.controller, snapshots.clone())
                });
            }
            Err(error) => {
                snapshots
                    .send(Update::Snapshot(current, Err(error.to_string())))
                    .await?;
            }
        }
        sleep(Duration::from_secs(1)).await;
    }
}

async fn poll(
    address: <PresentationAddress as ToOwned>::Owned,
    host: Uuid,
    controller: Uuid,
    snapshots: mpsc::Sender<Update>,
) {
    loop {
        let result = poll_connected(&address, host, controller, &snapshots).await;
        let message = match result {
            Ok(()) => "Notification connection closed".into(),
            Err(error) => error.to_string(),
        };
        if snapshots
            .send(Update::Snapshot(host, Err(message)))
            .await
            .is_err()
        {
            return;
        }
        sleep(Duration::from_secs(3)).await;
    }
}

async fn poll_connected(
    address: &PresentationAddress,
    host: Uuid,
    controller: Uuid,
    snapshots: &mpsc::Sender<Update>,
) -> Result<()> {
    let mut client = PresentationClient::connect(address).await?;
    ensure!(
        client.session().host_id == host.to_string(),
        "companion belongs to another host"
    );
    ensure!(
        client
            .session()
            .capabilities
            .iter()
            .any(|capability| capability == PRESENTATION_ATTENTION_CAPABILITY),
        "This companion does not support OS notification metadata. Update it to enable notifications."
    );
    loop {
        let reply = timeout(
            COMMAND_TIMEOUT,
            client.request(Body::GetPresentationAttention(GetPresentationAttention {})),
        )
        .await??;
        let Body::PresentationAttention(snapshot) = reply else {
            anyhow::bail!("unexpected notification reply");
        };
        ensure!(
            snapshot.controller_id == controller.to_string(),
            "unexpected companion controller"
        );
        snapshots.send(Update::Snapshot(host, Ok(snapshot))).await?;
        sleep(Duration::from_secs(1)).await;
    }
}
