#[cfg(windows)]
use std::path::Path;
use std::path::PathBuf;
#[cfg(not(windows))]
use std::{process::Stdio, time::Duration};

use anyhow::{Context, Result, ensure};
use corvid_client::ControlEndpoint;
#[cfg(windows)]
use corvid_companion::Config;
use corvid_companion::monitor::Scope;
use corvid_protocol::wire::DesktopAgentFocused;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault};
#[cfg(not(windows))]
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
#[cfg(not(windows))]
use tokio::time::sleep;
use tokio::time::timeout;
use uuid::Uuid;

use crate::connection::{self, Client};

pub use corvid_presentation::desktop_focus::FocusRequest;

#[derive(Default)]
pub struct Job {
    pub tasks: JoinSet<Result<()>>,
    show: Option<mpsc::Sender<()>>,
}

impl Job {
    pub fn spawn(
        &mut self,
        settings: Settings,
        endpoint: ControlEndpoint,
        host: String,
        focus: mpsc::Sender<FocusRequest>,
    ) {
        let (show, shown) = mpsc::channel(1);
        self.show = Some(show);
        self.tasks.spawn(async move {
            open(settings, endpoint, Uuid::parse_str(&host)?, shown, focus).await
        });
    }

    pub fn show(&self) {
        if let Some(show) = &self.show {
            let _ = show.try_send(());
        }
    }

    pub async fn close(&mut self) {
        self.show = None;
        // Retire only this Desktop registration. The separate Agents process keeps
        // observing its host and does not own the terminal writer's lifetime.
        self.tasks.shutdown().await;
    }
}

#[derive(Clone)]
pub struct Settings {
    pub state: PathBuf,
    #[cfg(windows)]
    pub pipe_instance: String,
}

impl From<PathBuf> for Settings {
    fn from(state: PathBuf) -> Self {
        Self {
            state,
            #[cfg(windows)]
            pipe_instance: "main".into(),
        }
    }
}

pub fn executable() -> Result<PathBuf> {
    corvid_update::layout::agent_deck_executable(&std::env::current_exe()?)
}

async fn open(
    settings: Settings,
    endpoint: ControlEndpoint,
    host: Uuid,
    shown: mpsc::Receiver<()>,
    focus: mpsc::Sender<FocusRequest>,
) -> Result<()> {
    let scope = settings.scope(&endpoint, host)?;
    let config = &scope.config;
    #[cfg(windows)]
    let client = {
        let mut client = crate::lifecycle::ensure_current(
            &executable()?,
            &scope.state,
            &scope.address,
            config,
            true,
        )
        .await?
        .context("Agent Deck was not started")?;
        client.activate(true).await?;
        client
    };
    #[cfg(not(windows))]
    // TODO: Refresh running Unix Agent Decks after a Desktop update
    // Add version-aware replacement for Linux and macOS while preserving independent Deck lifetime and shared companion state, matching the Windows startup and activation behavior.
    let client = match Client::connect(host, config.controller_id, true).await {
        Ok(client) => client,
        Err(_) => {
            let mut child = Command::new(executable()?);
            child.arg("--state-dir").arg(&scope.state);
            #[cfg(windows)]
            child.arg("--pipe-instance").arg(&scope.address);
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            child.arg("--socket-dir").arg(&scope.address);
            child
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(windows)]
            child.creation_flags(0x0800_0000);
            let mut process = child.spawn().context("starting Corvid Agent Deck")?;
            timeout(COMMAND_TIMEOUT, async {
                loop {
                    if let Ok(client) = Client::connect(host, config.controller_id, true).await {
                        return Ok::<_, anyhow::Error>(client);
                    }
                    if let Some(status) = process.try_wait()? {
                        anyhow::bail!("Corvid Agent Deck exited during startup ({status})");
                    }
                    sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .context("Corvid Agent Deck did not become ready")??
        }
    };
    forward_focus(client, shown, focus).await
}

async fn forward_focus(
    mut client: Client,
    mut shown: mpsc::Receiver<()>,
    focus: mpsc::Sender<FocusRequest>,
) -> Result<()> {
    loop {
        let request = tokio::select! {
            shown = shown.recv() => {
                if shown.is_none() { return Ok(()); }
                client.open(false).await?;
                continue;
            }
            request = client.reader.next_envelope() => request?,
        };
        let Some(request) = request else {
            return Ok(());
        };
        client.session.validate_scope(&request)?;
        let id = connection::request_id(&request)?.to_string();
        match request.body {
            Some(Body::AgentDeckOpened(_)) => continue,
            Some(Body::FocusDesktopAgent(request)) => {
                let run = request.run.context("missing Agent Deck run")?;
                ensure!(
                    run.host_id == client.session.host_id,
                    "Agent Deck focus belongs to another host"
                );
                let (result, done) = oneshot::channel();
                let outcome = timeout(COMMAND_TIMEOUT, async {
                    focus
                        .send(FocusRequest { run, result })
                        .await
                        .context("Desktop focus channel closed")?;
                    done.await.context("Desktop focus result unavailable")
                })
                .await;
                let body = match outcome {
                    Ok(Ok(Ok(()))) => Body::DesktopAgentFocused(DesktopAgentFocused {}),
                    Ok(Ok(Err(error))) => Body::Error(ProtocolFault::new("FocusUnavailable", &error).into_wire()),
                    _ => Body::Error(ProtocolFault::new("FocusUnavailable", "The registered Desktop could not focus this run. Check the current terminal.").into_wire()),
                };
                client.reply(id, body).await?;
            }
            _ => anyhow::bail!("unsupported Agent Deck request"),
        }
    }
}

impl Settings {
    /// Refresh already-running Windows Decks without opening stopped ones.
    #[cfg(windows)]
    pub async fn refresh_running(&self, executable: &Path) -> Result<()> {
        let mut states = vec![(self.state.clone(), Some(self.pipe_instance.clone()), None)];
        match std::fs::read_dir(self.state.join("slot-board")) {
            Ok(entries) => {
                for entry in entries.take(256) {
                    let entry = entry?;
                    if entry.file_type()?.is_dir()
                        && let Some(host) = entry
                            .file_name()
                            .to_str()
                            .and_then(|name| Uuid::parse_str(name).ok())
                    {
                        states.push((entry.path(), None, Some(host)));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut failures = Vec::new();
        for (state, address, host) in states {
            let result = async {
                let Some(config) = Config::read_if_present(&state)? else {
                    return Ok(());
                };
                if host.is_some_and(|host| host != config.host_id) {
                    return Ok(());
                }
                let address = address.unwrap_or_else(|| config.controller_id.simple().to_string());
                crate::lifecycle::ensure_current(executable, &state, &address, &config, false)
                    .await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                failures.push(error.to_string());
            }
        }
        ensure!(
            failures.is_empty(),
            "Could not update running Agent Decks: {}",
            failures.join("; ")
        );
        Ok(())
    }

    #[cfg(windows)]
    pub fn refresh_in_background(&self) -> Result<()> {
        let settings = self.clone();
        let executable = executable()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        std::thread::Builder::new()
            .name("corvid-agent-deck-update".into())
            .spawn(move || {
                if let Err(error) = runtime.block_on(settings.refresh_running(&executable)) {
                    eprintln!("{error:#}");
                }
            })?;
        Ok(())
    }

    pub fn scope(&self, endpoint: &ControlEndpoint, host: Uuid) -> Result<Scope> {
        #[cfg(windows)]
        let instance = self.pipe_instance.as_str();
        #[cfg(not(windows))]
        let instance = "main";
        Scope::prepare(&self.state, endpoint, host, instance)
    }
}
