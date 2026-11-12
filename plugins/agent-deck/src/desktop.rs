use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_client::ControlEndpoint;
use corvid_companion::{Config, HostEndpoint, PresentationAddress};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{DesktopAgentFocused, PresentedRunKey};
use corvid_protocol::{COMMAND_TIMEOUT, ProtocolFault};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use crate::connection::{self, Client};

const DEFAULT_SLOT_COUNT: u32 = 8;

pub struct FocusRequest {
    pub run: PresentedRunKey,
    pub result: oneshot::Sender<Result<(), String>>,
}

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

struct Scope {
    address: <PresentationAddress as ToOwned>::Owned,
    state: PathBuf,
}

pub fn executable() -> Result<PathBuf> {
    let current = std::env::current_exe()?;
    let directory = current.parent().context("locating Corvid installation")?;
    let binary = format!("corvid-agent-deck{}", std::env::consts::EXE_SUFFIX);
    let packaged = directory.join("plugins/agent-deck").join(&binary);
    if packaged.is_file() {
        return Ok(packaged);
    }
    let development = directory.join(binary);
    ensure!(
        development.is_file(),
        "The Agents window is missing from this installation. Reinstall Corvid Desktop to restore it."
    );
    Ok(development)
}

async fn open(
    settings: Settings,
    endpoint: ControlEndpoint,
    host: Uuid,
    shown: mpsc::Receiver<()>,
    focus: mpsc::Sender<FocusRequest>,
) -> Result<()> {
    let saved = match endpoint {
        ControlEndpoint::Local(runtime_dir) => HostEndpoint::Local { runtime_dir },
        ControlEndpoint::Ssh(alias) => HostEndpoint::Ssh {
            alias: alias.as_str().to_owned(),
        },
        ControlEndpoint::Wsl(distribution) => HostEndpoint::Wsl {
            distribution: distribution.as_str().to_owned(),
        },
    };
    let (scope, config) = settings.scope(saved, host)?;
    let client = match Client::connect(host, config.controller_id, true).await {
        Ok(client) => client,
        Err(_) => {
            let mut child = Command::new(executable()?);
            child.arg("--state-dir").arg(&scope.state);
            #[cfg(windows)]
            child.arg("--pipe-instance").arg(&scope.address);
            #[cfg(target_os = "linux")]
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
                    _ => Body::Error(ProtocolFault::new("FocusUnavailable", "The registered Desktop could not focus this run. Check the current terminal.").into_wire()),
                };
                client.reply(id, body).await?;
            }
            _ => anyhow::bail!("unsupported Agent Deck request"),
        }
    }
}

impl Settings {
    fn scope(&self, endpoint: HostEndpoint, host: Uuid) -> Result<(Scope, Config)> {
        let shared = Config::read_if_present(&self.state)?
            .is_some_and(|config| config.host_id == host && config.endpoint == endpoint);
        let state = if shared {
            self.state.clone()
        } else {
            self.state
                .join("slot-board")
                .join(host.simple().to_string())
        };
        let config = prepare_state(&state, endpoint, host)?;
        #[cfg(windows)]
        let address = if shared {
            self.pipe_instance.clone()
        } else {
            config.controller_id.simple().to_string()
        };
        #[cfg(target_os = "linux")]
        let address = {
            let runtime = std::env::var_os("XDG_RUNTIME_DIR")
                .filter(|path| !path.is_empty())
                .context("set XDG_RUNTIME_DIR to open Corvid Agent Deck")?;
            let directory = if shared {
                "corvid-companion".to_owned()
            } else {
                format!("cv-slot-{}", config.controller_id.simple())
            };
            PathBuf::from(runtime).join(directory)
        };
        Ok((Scope { address, state }, config))
    }
}

fn prepare_state(state: &Path, endpoint: HostEndpoint, host: Uuid) -> Result<Config> {
    let config = match Config::read_if_present(state)? {
        Some(config) => config,
        None => {
            let created = corvid_companion::initialize(state, host, endpoint, DEFAULT_SLOT_COUNT);
            match Config::read_if_present(state)? {
                Some(config) => config,
                None => {
                    created?;
                    anyhow::bail!("Corvid Agent Deck companion configuration is missing");
                }
            }
        }
    };
    ensure!(
        config.host_id == host,
        "Corvid Agent Deck companion configuration belongs to a different host"
    );
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_the_controller_and_refuses_a_different_host() -> Result<()> {
        #[cfg(target_os = "linux")]
        let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
        #[cfg(windows)]
        let directory = tempfile::tempdir()?;
        let state = directory.path().join("slot-board");
        let host = Uuid::new_v4();
        let endpoint = HostEndpoint::Ssh {
            alias: "work-host".into(),
        };
        let first = prepare_state(&state, endpoint.clone(), host)?;
        let reopened = prepare_state(&state, endpoint.clone(), host)?;
        assert_eq!(first.controller_id, reopened.controller_id);
        assert!(prepare_state(&state, endpoint, Uuid::new_v4()).is_err());
        assert_eq!(Config::read_if_present(&state)?.unwrap().host_id, host);
        Ok(())
    }
}
