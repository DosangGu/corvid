use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_agent_deck::connection;
use corvid_companion::{
    Config, PRESENTATION_ATTENTION_CAPABILITY, PresentationAddress, PresentationClient,
};
use corvid_presentation::ipc::Completion;
use corvid_protocol::COMMAND_TIMEOUT;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{GetPresentationAttention, PresentationAttention};
use eframe::egui;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use crate::notifications::{Ledger, Notice, Settings, now_ms};
use crate::parent::{self, DesktopLink};
use crate::platform::{Event, Native};

#[cfg(target_os = "linux")]
use corvid_protocol::unix::RuntimeDir as StateDirectory;
#[cfg(windows)]
use corvid_protocol::windows::StateDirectory;

#[derive(Clone, Default)]
pub struct View {
    pub settings: Settings,
    pub history: Vec<Notice>,
    pub selected: Option<Notice>,
    pub status: String,
    pub background: bool,
    pub notifications_open: u64,
    pub quit: bool,
}

pub struct Background {
    pub desktop: DesktopLink,
    pub completed: mpsc::Receiver<Completion>,
    pub view: watch::Receiver<View>,
    settings: watch::Sender<Settings>,
    test: mpsc::Sender<()>,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl Background {
    pub fn start(
        state: PathBuf,
        address: <PresentationAddress as ToOwned>::Owned,
        config: Config,
        ctx: egui::Context,
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
                    };
                    let job = run(state, address, config, channels);
                    tokio::select! {
                        result = job => if let Err(error) = result {
                            let _ = started.try_send(Err(error.to_string()));
                            views.send_modify(|view| {
                                view.status = error.to_string();
                                view.background = false;
                            });
                            parent::show(&ctx);
                        },
                        _ = stopped => {}
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
            completed: completions,
            view,
            settings,
            test,
            stop: Some(stop),
            worker: Some(worker),
        })
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
    ctx: egui::Context,
    started: std::sync::mpsc::SyncSender<Result<(), String>>,
}

async fn run(
    state: PathBuf,
    address: <PresentationAddress as ToOwned>::Owned,
    config: Config,
    channels: Channels,
) -> Result<()> {
    let Channels {
        bridge,
        completed,
        views,
        mut settings,
        mut tests,
        ctx,
        started,
    } = channels;
    let storage = StateDirectory::create_all(&state.join("agent-deck"))?;
    let _lock = storage.lock_storage()?;
    let mut ledger: Ledger = storage
        .read_private_file("notifications.json", 1024 * 1024)?
        .map(|bytes| serde_json::from_slice(&bytes))
        .transpose()?
        .unwrap_or_default();
    ledger.validate()?;
    let (events, mut event_rx) = mpsc::channel(32);
    let native = Native::start(
        events.clone(),
        ctx.clone(),
        config.controller_id,
        &state,
        &address,
    );
    let (native, mut issue) = match native {
        Ok(native) => (Some(native), String::new()),
        Err(error) => (None, error.to_string()),
    };
    if ledger.settings.start_at_login
        && let Some(native) = &native
        && let Err(error) = native.startup(true)
    {
        issue = error.to_string();
        ledger.settings.start_at_login = false;
        save(&storage, &ledger)?;
    }
    views.send_replace(View {
        settings: ledger.settings.clone(),
        history: ledger.history.iter().rev().cloned().collect(),
        status: issue.clone(),
        background: native.as_ref().is_some_and(Native::background),
        ..Default::default()
    });
    let (peers, mut peer_rx) = mpsc::channel(8);
    let (listening, listening_rx) = oneshot::channel();
    let mut jobs = tokio::task::JoinSet::new();
    jobs.spawn(connection::listen(config.controller_id, peers, listening));
    tokio::select! {
        result = listening_rx => result.context("Agent Deck instance already exists or its private endpoint is unavailable")?,
        result = jobs.join_next() => { anyhow::bail!("Agent Deck endpoint unavailable: {result:?}"); }
    }
    let (snapshots, mut snapshot_rx) = mpsc::channel(1);
    jobs.spawn(poll(
        state,
        address,
        config.host_id,
        config.controller_id,
        snapshots,
    ));
    let epoch = Uuid::new_v4();
    let mut clients = tokio::task::JoinSet::new();
    let mut monitor = Monitor {
        host: config.host_id,
        storage,
        ledger,
        native,
        issue,
        views,
        ctx,
    };
    let _ = started.send(Ok(()));
    loop {
        tokio::select! {
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
            result = snapshot_rx.recv() => {
                monitor.snapshot(result.context("notification poller stopped")?).await?;
            }
            result = settings.changed() => {
                result?;
                monitor.configure(settings.borrow_and_update().clone())?;
            }
            test = tests.recv() => {
                test.context("notification test channel closed")?;
                monitor.notify(test_notice(config.host_id)).await;
            }
            event = event_rx.recv() => {
                monitor.event(event.context("OS event channel closed")?)?;
            }
        }
    }
}

struct Monitor {
    host: Uuid,
    storage: StateDirectory,
    ledger: Ledger,
    native: Option<Native>,
    issue: String,
    views: watch::Sender<View>,
    ctx: egui::Context,
}

impl Monitor {
    async fn snapshot(&mut self, result: Result<PresentationAttention, String>) -> Result<()> {
        let snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(issue) => {
                if let Some(native) = &self.native {
                    native.status("Corvid Agent Deck — companion unavailable", false);
                }
                self.views.send_modify(|view| view.status = issue);
                self.ctx.request_repaint();
                return Ok(());
            }
        };
        let observation = self.ledger.observe(self.host, &snapshot, now_ms())?;
        if observation.changed {
            save(&self.storage, &self.ledger)?;
        }
        let status = if snapshot.omitted_count > 0 {
            format!(
                "{} older notifications exceed the bounded feed.",
                snapshot.omitted_count
            )
        } else {
            self.issue.clone()
        };
        self.views.send_modify(|view| {
            view.history = self.ledger.history.iter().rev().cloned().collect();
            view.status = status;
        });
        if let Some(native) = &self.native {
            let state = if snapshot.control_generation.is_some() {
                "connected"
            } else {
                "offline"
            };
            native.status(
                &format!(
                    "Corvid Agent Deck — {state} — {} unseen",
                    snapshot.attention.len()
                ),
                !snapshot.attention.is_empty(),
            );
        }
        for notice in batch(observation.notices) {
            self.notify(notice).await;
        }
        self.ctx.request_repaint();
        Ok(())
    }

    async fn notify(&mut self, notice: Notice) {
        let result = match &self.native {
            Some(native) => native.notify(notice, self.ledger.settings.sound).await,
            None => Err(anyhow::anyhow!("OS notifications unavailable")),
        };
        if let Err(error) = result {
            self.error(error.to_string());
        }
        self.ctx.request_repaint();
    }

    fn configure(&mut self, mut next: Settings) -> Result<()> {
        if next.start_at_login != self.ledger.settings.start_at_login {
            let result = self
                .native
                .as_ref()
                .context("OS integration unavailable")
                .and_then(|native| native.startup(next.start_at_login));
            if let Err(error) = result {
                next.start_at_login = self.ledger.settings.start_at_login;
                self.error(error.to_string());
            }
        }
        self.ledger.settings = next;
        save(&self.storage, &self.ledger)?;
        self.views
            .send_modify(|view| view.settings = self.ledger.settings.clone());
        self.ctx.request_repaint();
        Ok(())
    }

    fn error(&mut self, issue: String) {
        self.issue = issue;
        self.views
            .send_modify(|view| view.status = self.issue.clone());
    }

    fn event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Open => parent::show(&self.ctx),
            Event::Notifications => self.open_notice(None),
            Event::Notice(notice) => self.open_notice(Some(notice)),
            Event::Pause => {
                let mut settings = self.ledger.settings.clone();
                settings.paused_until_ms = if settings.paused_until_ms > now_ms() {
                    0
                } else {
                    now_ms().saturating_add(60 * 60 * 1000)
                };
                self.configure(settings)?;
            }
            Event::Quit => {
                self.views.send_modify(|view| view.quit = true);
                self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Event::Error(error) => self.error(error),
        }
        self.ctx.request_repaint();
        Ok(())
    }

    fn open_notice(&self, notice: Option<Notice>) {
        self.views.send_modify(|view| {
            view.notifications_open += 1;
            view.selected = notice;
        });
        parent::show(&self.ctx);
    }
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

fn test_notice(host: Uuid) -> Notice {
    Notice {
        summary: true,
        key: crate::notifications::Key {
            host,
            epoch: Uuid::new_v4(),
            run: Uuid::new_v4(),
            attention: Uuid::new_v4(),
        },
        title: "Corvid Agent Deck test notification".into(),
        body: "Notifications are working. Click to open notification settings.".into(),
        created_ms: now_ms(),
    }
}

fn save(storage: &StateDirectory, ledger: &Ledger) -> Result<()> {
    storage.replace_private_file("notifications.json", &serde_json::to_vec(ledger)?)
}

async fn poll(
    state: PathBuf,
    address: <PresentationAddress as ToOwned>::Owned,
    host: Uuid,
    controller: Uuid,
    snapshots: mpsc::Sender<Result<PresentationAttention, String>>,
) -> Result<()> {
    loop {
        let result = poll_connected(&state, &address, host, controller, &snapshots).await;
        let message = match result {
            Ok(()) => "Notification connection closed".into(),
            Err(error) => error.to_string(),
        };
        snapshots.send(Err(message)).await?;
        sleep(Duration::from_secs(3)).await;
    }
}

async fn poll_connected(
    state: &Path,
    address: &PresentationAddress,
    host: Uuid,
    controller: Uuid,
    snapshots: &mpsc::Sender<Result<PresentationAttention, String>>,
) -> Result<()> {
    let mut client = match PresentationClient::connect_if_present(address).await? {
        Some(client) => client,
        None => {
            let mut child = tokio::process::Command::new(std::env::current_exe()?);
            child
                .arg("--companion-service")
                .arg("--state-dir")
                .arg(state);
            #[cfg(windows)]
            child
                .arg("--pipe-instance")
                .arg(address)
                .creation_flags(0x0800_0000);
            #[cfg(target_os = "linux")]
            child.arg("--socket-dir").arg(address).process_group(0);
            let mut process = child
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            timeout(COMMAND_TIMEOUT, async {
                loop {
                    if let Some(client) = PresentationClient::connect_if_present(address).await? {
                        return Ok::<_, anyhow::Error>(client);
                    }
                    if let Some(status) = process.try_wait()? {
                        anyhow::bail!("companion service exited ({status})");
                    }
                    sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .context("companion startup timed out")??
        }
    };
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
        snapshots.send(Ok(snapshot)).await?;
        sleep(Duration::from_secs(1)).await;
    }
}
