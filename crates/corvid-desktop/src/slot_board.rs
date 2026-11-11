use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_client::ControlEndpoint;
use corvid_companion::{Config, HostEndpoint, PresentationAddress, PresentationClient};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{DesktopAgentFocused, Envelope, PresentedRunKey};
use corvid_protocol::{
    COMMAND_TIMEOUT, FrameReader, FrameRole, FrameWriter, PROTOCOL_MAJOR, ProtocolFault,
};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

const DEFAULT_SLOT_COUNT: u32 = 8;
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) struct FocusRequest {
    pub run: PresentedRunKey,
    pub result: oneshot::Sender<Result<(), String>>,
}

#[derive(Default)]
pub(crate) struct Job {
    pub tasks: JoinSet<Result<()>>,
    close: Option<oneshot::Sender<()>>,
}

impl Job {
    pub fn spawn(
        &mut self,
        settings: Settings,
        endpoint: ControlEndpoint,
        host: String,
        focus: mpsc::Sender<FocusRequest>,
    ) {
        let (close, closing) = oneshot::channel();
        self.close = Some(close);
        self.tasks.spawn(async move {
            open(settings, endpoint, Uuid::parse_str(&host)?, closing, focus).await
        });
    }

    pub async fn close(&mut self) {
        if let Some(close) = self.close.take() {
            let _ = close.send(());
        }
        if timeout(CLOSE_TIMEOUT, self.tasks.join_next())
            .await
            .is_err()
        {
            self.tasks.shutdown().await;
        }
    }
}

#[derive(Clone)]
pub(crate) struct Settings {
    pub state: PathBuf,
    #[cfg(windows)]
    pub pipe_instance: String,
}

struct Scope {
    address: <PresentationAddress as ToOwned>::Owned,
    state: PathBuf,
}

async fn open(
    settings: Settings,
    endpoint: ControlEndpoint,
    host: Uuid,
    closing: oneshot::Receiver<()>,
    focus: mpsc::Sender<FocusRequest>,
) -> Result<()> {
    let executable = std::env::current_exe()?
        .parent()
        .context("locating the Corvid Desktop installation")?
        .join("plugins/slot-board")
        .join(format!("corvid-slot-board{}", std::env::consts::EXE_SUFFIX));
    ensure!(
        executable.is_file(),
        "Agent Board is unavailable. Install its plugin beside Corvid Desktop."
    );

    let saved = match &endpoint {
        ControlEndpoint::Local(runtime) => HostEndpoint::Local {
            runtime_dir: runtime.clone(),
        },
        ControlEndpoint::Ssh(alias) => HostEndpoint::Ssh {
            alias: alias.as_str().to_owned(),
        },
        ControlEndpoint::Wsl(distribution) => HostEndpoint::Wsl {
            distribution: distribution.as_str().to_owned(),
        },
    };
    let (scope, config) = settings.scope(saved, host)?;
    if let Some(client) = PresentationClient::connect_if_present(&scope.address).await? {
        verify(client, &config).await?;
        return show(&executable, &scope, &config, closing, focus).await;
    }

    let (stop, stopped) = oneshot::channel();
    let companion =
        corvid_companion::run_for_endpoint(&scope.address, &scope.state, endpoint, host, async {
            let _ = stopped.await;
        });
    let window = async {
        let client = wait_for_companion(&scope.address).await?;
        verify(client, &config).await?;
        show(&executable, &scope, &config, closing, focus).await
    };
    tokio::pin!(companion, window);
    let result = tokio::select! {
        result = &mut companion => return result,
        result = &mut window => result,
    };
    let _ = stop.send(());
    companion.await?;
    result
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
                .context("set XDG_RUNTIME_DIR to open Agent Board")?;
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
                    anyhow::bail!("Agent Board companion configuration is missing");
                }
            }
        }
    };
    ensure!(
        config.host_id == host,
        "Agent Board companion configuration belongs to a different host"
    );
    Ok(config)
}

async fn wait_for_companion(address: &PresentationAddress) -> Result<PresentationClient> {
    timeout(COMMAND_TIMEOUT, async {
        loop {
            if let Some(client) = PresentationClient::connect_if_present(address).await? {
                return Ok::<_, anyhow::Error>(client);
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("Agent Board companion did not start")?
}

async fn verify(mut client: PresentationClient, config: &Config) -> Result<()> {
    timeout(COMMAND_TIMEOUT, client.slots(Some(config)))
        .await
        .context("Agent Board companion identity query timed out")??;
    Ok(())
}

async fn show(
    executable: &Path,
    scope: &Scope,
    config: &Config,
    mut closing: oneshot::Receiver<()>,
    focus: mpsc::Sender<FocusRequest>,
) -> Result<()> {
    if !matches!(closing.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
        return Ok(());
    }
    let mut command = Command::new(executable);
    command
        .arg("--desktop-pid")
        .arg(std::process::id().to_string())
        .arg("--host-id")
        .arg(config.host_id.to_string())
        .arg("--controller-id")
        .arg(config.controller_id.to_string());
    #[cfg(windows)]
    command.arg("--pipe-instance").arg(&scope.address);
    #[cfg(target_os = "linux")]
    command.arg("--socket-dir").arg(&scope.address);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("opening Agent Board")?;
    let mut input = child
        .stdin
        .take()
        .context("missing Agent Board reply pipe")?;
    let output = child
        .stdout
        .take()
        .context("missing Agent Board request pipe")?;
    let status = tokio::select! {
        status = child.wait() => status,
        result = forward_focus(&mut input, output, focus) => {
            result?;
            drop(input);
            child.wait().await
        },
        _ = &mut closing => { drop(input); child.wait().await }
    }
    .context("waiting for Agent Board")?;
    ensure!(status.success(), "Agent Board exited with {status}");
    Ok(())
}

async fn forward_focus(
    input: &mut tokio::process::ChildStdin,
    output: tokio::process::ChildStdout,
    focus: mpsc::Sender<FocusRequest>,
) -> Result<()> {
    let mut writer = FrameWriter::start(input, FrameRole::StructuredOnly).await?;
    let mut reader = FrameReader::start(output, FrameRole::StructuredOnly).await?;
    while let Some(request) = reader.next_envelope().await? {
        let id = request
            .request_id
            .context("Agent Board request has no ID")?;
        ensure!(
            request.protocol_major == PROTOCOL_MAJOR && Uuid::parse_str(&id).is_ok(),
            "invalid Agent Board request header"
        );
        let Some(Body::FocusDesktopAgent(request)) = request.body else {
            anyhow::bail!("unsupported Agent Board request");
        };
        let run = request.run.context("Agent Board request has no run")?;
        let (result, done) = oneshot::channel();
        if focus.send(FocusRequest { run, result }).await.is_err() {
            return Ok(());
        }
        let Ok(outcome) = done.await else {
            return Ok(());
        };
        let body = match outcome {
            Ok(()) => Body::DesktopAgentFocused(DesktopAgentFocused {}),
            Err(message) => {
                Body::Error(ProtocolFault::new("FocusUnavailable", &message).into_wire())
            }
        };
        writer
            .envelope(&Envelope {
                protocol_major: PROTOCOL_MAJOR,
                request_id: Some(id),
                body: Some(body),
                ..Default::default()
            })
            .await?;
    }
    Ok(())
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
