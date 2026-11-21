use anyhow::{Context, Result, ensure};
use corvid_async::{mpsc, oneshot};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{DesktopAgentFocused, FocusDesktopAgent, PresentedRunKey};
use corvid_protocol::{
    COMMAND_TIMEOUT, FrameReader, FrameRole, FrameWriter, PeerRole, ProtocolFault, accept_hello,
    check_hello_result, hello,
};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use tokio::task::JoinSet;
use tokio::time::timeout;
use uuid::Uuid;

const CAPABILITY: &str = "desktop_focus.v1";
const ROUTED_CAPABILITY: &str = "desktop_route.v1";
const ROUTED_TIMEOUT: Duration = Duration::from_secs(20);
#[cfg(windows)]
const PIPE_PREFIX: &str = "corvid-desktop-focus-v1";
#[cfg(windows)]
const ROUTED_PREFIX: &str = "corvid-desktop-route-v1";
const MAX_PEERS: usize = 4;

trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}
type Stream = Box<dyn Duplex>;

pub struct FocusRequest {
    pub run: PresentedRunKey,
    pub result: FocusReply,
}

pub type FocusReply = oneshot::Sender<Result<(), FocusFailure>>;

#[derive(Debug)]
pub struct FocusFailure(ProtocolFault);

impl From<ProtocolFault> for FocusFailure {
    fn from(fault: ProtocolFault) -> Self {
        Self(fault)
    }
}

impl From<&str> for FocusFailure {
    fn from(message: &str) -> Self {
        Self(ProtocolFault::new("FocusUnavailable", message))
    }
}

impl FocusFailure {
    pub fn into_wire(self) -> corvid_protocol::wire::RpcError {
        self.0.into_wire()
    }
}

async fn frames(
    stream: Stream,
) -> Result<(
    FrameReader<ReadHalf<Stream>>,
    FrameWriter<WriteHalf<Stream>>,
)> {
    let (read, write) = tokio::io::split(stream);
    let writer = FrameWriter::start(write, FrameRole::StructuredOnly).await?;
    let reader = FrameReader::start(read, FrameRole::StructuredOnly).await?;
    Ok((reader, writer))
}

/// Request exact navigation in a running Desktop.
pub async fn request(controller: Uuid, run: PresentedRunKey) -> Result<()> {
    request_with_activation(controller, run, |_| async {}).await
}

/// Run the activation handoff with the verified Desktop process ID before requesting focus.
pub async fn request_with_activation(
    controller: Uuid,
    run: PresentedRunKey,
    handoff: impl AsyncFnOnce(u32),
) -> Result<()> {
    let (stream, pid, routed) = match timeout(COMMAND_TIMEOUT, connect(controller, true)).await? {
        Ok((stream, pid)) => (stream, pid, true),
        Err(error) if error.is::<EndpointUnavailable>() || missing_endpoint(&error) => {
            match timeout(COMMAND_TIMEOUT, connect(controller, false)).await? {
                Err(error) if missing_endpoint(&error) => return Err(EndpointUnavailable.into()),
                result => {
                    let (stream, pid) = result?;
                    (stream, pid, false)
                }
            }
        }
        Err(error) => return Err(error),
    };
    let wait = if routed {
        ROUTED_TIMEOUT
    } else {
        COMMAND_TIMEOUT
    };
    timeout(wait, async {
        request_on_stream(stream, controller, run, routed, async move {
            if let Some(pid) = pid {
                handoff(pid).await;
            }
        })
        .await
    })
    .await
    .context("Desktop did not confirm focus in time")?
}

#[derive(Debug)]
pub struct EndpointUnavailable;

impl std::fmt::Display for EndpointUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Open this host in Corvid Desktop to focus its agents.")
    }
}

impl std::error::Error for EndpointUnavailable {}

fn missing_endpoint(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        )
    })
}

async fn request_on_stream(
    stream: Stream,
    controller: Uuid,
    run: PresentedRunKey,
    routed: bool,
    handoff: impl Future<Output = ()>,
) -> Result<()> {
    let (mut reader, mut writer) = frames(stream).await?;
    let capability = if routed {
        ROUTED_CAPABILITY
    } else {
        CAPABILITY
    };
    let greeting = hello(
        Uuid::new_v4().to_string(),
        Some(controller.to_string()),
        vec![capability.into()],
    );
    writer.envelope(&greeting).await?;
    let reply = reader
        .next_envelope()
        .await?
        .context("Desktop closed during Hello")?;
    let session = check_hello_result(&greeting, &reply, PeerRole::Presentation)?;
    ensure!(
        session.host_id == run.host_id
            && session.capabilities.iter().any(|cap| cap == capability)
            && if routed {
                session.server_epoch == controller.to_string()
            } else {
                session.server_epoch == run.remote_epoch
            },
        "The agent belongs to a different daemon session."
    );
    handoff.await;
    let id = Uuid::new_v4().to_string();
    writer
        .envelope(&session.envelope(
            Some(id.clone()),
            Body::FocusDesktopAgent(FocusDesktopAgent { run: Some(run) }),
        ))
        .await?;
    let reply = reader
        .next_envelope()
        .await?
        .context("Desktop did not confirm focus")?;
    session.validate_scope(&reply)?;
    ensure!(
        reply.request_id.as_deref() == Some(id.as_str()),
        "mismatched Desktop response"
    );
    match reply.body {
        Some(Body::DesktopAgentFocused(_)) => Ok(()),
        Some(Body::Error(error)) => Err(ProtocolFault::from_wire(&error).into()),
        _ => anyhow::bail!("unexpected Desktop focus response"),
    }
}

#[cfg(windows)]
async fn connect(controller: Uuid, routed: bool) -> Result<(Stream, Option<u32>)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

    let prefix = if routed { ROUTED_PREFIX } else { PIPE_PREFIX };
    let pipe =
        corvid_protocol::windows::open_scoped(prefix, &controller.simple().to_string()).await?;
    let mut pid = 0;
    ensure!(
        unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } != 0 && pid != 0,
        "could not identify the Desktop process"
    );
    // On a click, Deck can pass its foreground permission to the private
    // endpoint's actual owner. The Desktop UI reports any refusal to activate.
    unsafe { AllowSetForegroundWindow(pid) };
    Ok((Box::new(pipe), Some(pid)))
}

#[cfg(not(windows))]
async fn connect(controller: Uuid, routed: bool) -> Result<(Stream, Option<u32>)> {
    use corvid_protocol::unix::{RuntimeDir, SocketRole, peer_pid};

    let path = socket(controller, routed)?;
    if !path.try_exists()? {
        return Err(EndpointUnavailable.into());
    }
    let stream = RuntimeDir::open(&path)?
        .connect(SocketRole::Presentation)
        .await?;
    let pid = peer_pid(&stream).ok();
    Ok((Box::new(stream), pid))
}

#[cfg(not(windows))]
fn socket(controller: Uuid, routed: bool) -> Result<std::path::PathBuf> {
    if routed {
        corvid_protocol::paths::desktop_route_socket(controller)?.prepare()
    } else {
        corvid_protocol::paths::desktop_focus_socket(controller)?.prepare()
    }
}

/// Keep focus available for a known host even while its terminal is detached.
pub async fn serve_routed(
    host: Uuid,
    controller: Uuid,
    focus: mpsc::Sender<FocusRequest>,
) -> Result<()> {
    // The private connection is scoped to the controller. Desktop verifies the
    // daemon epoch in each run against the actual daemon before navigation.
    serve_scope(host, controller, controller, true, focus).await
}

/// This listener is owned by an already attached Desktop session.
pub async fn serve(
    host: Uuid,
    controller: Uuid,
    epoch: Uuid,
    focus: mpsc::Sender<FocusRequest>,
) -> Result<()> {
    serve_scope(host, controller, epoch, false, focus).await
}

async fn serve_scope(
    host: Uuid,
    controller: Uuid,
    epoch: Uuid,
    routed: bool,
    focus: mpsc::Sender<FocusRequest>,
) -> Result<()> {
    #[cfg(windows)]
    let mut listener = corvid_protocol::windows::PipeListener::bind_scoped(
        if routed { ROUTED_PREFIX } else { PIPE_PREFIX },
        &controller.simple().to_string(),
    )?;
    #[cfg(not(windows))]
    use corvid_protocol::unix::{RuntimeDir, SocketRole};
    #[cfg(not(windows))]
    let directory = RuntimeDir::create_all(&socket(controller, routed)?)?;
    #[cfg(not(windows))]
    let instance = directory.lock_instance()?;
    #[cfg(not(windows))]
    let listener = instance.bind(SocketRole::Presentation).await?;
    let mut peers = JoinSet::new();
    loop {
        corvid_async::select! {
            _ = focus.closed() => return Ok(()),
            _ = peers.join_next() if !peers.is_empty() => {}
            connected = listener.accept() if peers.len() < MAX_PEERS => {
                #[cfg(windows)]
                let stream = connected?;
                #[cfg(not(windows))]
                let Some(stream) = connected? else { continue };
                let focus = focus.clone();
                peers.spawn(async move {
                    let wait = if routed { ROUTED_TIMEOUT } else { COMMAND_TIMEOUT };
                    let _ = timeout(wait, exchange(
                        Box::new(stream), host, controller, epoch, routed, focus,
                    )).await;
                });
            }
        }
    }
}

async fn exchange(
    stream: Stream,
    host: Uuid,
    controller: Uuid,
    epoch: Uuid,
    routed: bool,
    focus: mpsc::Sender<FocusRequest>,
) -> Result<()> {
    let (mut reader, mut writer) = frames(stream).await?;
    let greeting = reader
        .next_envelope()
        .await?
        .context("missing Desktop focus Hello")?;
    let (session, reply) = accept_hello(
        &greeting,
        PeerRole::Presentation,
        &host.to_string(),
        &epoch.to_string(),
        &[if routed {
            ROUTED_CAPABILITY
        } else {
            CAPABILITY
        }],
    )?;
    ensure!(
        session.controller_id.as_deref() == Some(controller.to_string().as_str()),
        "wrong controller"
    );
    writer.envelope(&reply).await?;
    let request = reader
        .next_envelope()
        .await?
        .context("missing Desktop focus request")?;
    session.validate_scope(&request)?;
    ensure!(
        request.sequence.is_none() && request.subscription_id.is_none(),
        "Desktop focus request contains event metadata"
    );
    let id = Uuid::parse_str(
        request
            .request_id
            .as_deref()
            .context("missing request ID")?,
    )?;
    ensure!(!id.is_nil(), "invalid Desktop focus request ID");
    let Some(Body::FocusDesktopAgent(body)) = request.body else {
        anyhow::bail!("unsupported Desktop focus request");
    };
    let run = body.run.context("missing agent identity")?;
    ensure!(
        run.host_id == session.host_id
            && (routed || run.remote_epoch == session.server_epoch)
            && Uuid::parse_str(&run.remote_epoch).is_ok_and(|epoch| !epoch.is_nil())
            && Uuid::parse_str(&run.agent_run_id).is_ok_and(|run| !run.is_nil()),
        "invalid Desktop focus scope"
    );
    let (result, completed) = oneshot::channel();
    let outcome = match focus.try_send(FocusRequest { run, result }) {
        Ok(()) => corvid_async::select! {
            result = completed => result.context("Desktop focus result unavailable")?,
            next = reader.next_envelope() => {
                ensure!(next?.is_none(), "unexpected second Desktop focus request");
                return Ok(());
            }
        },
        Err(_) => Err("Desktop is unavailable or handling another focus request.".into()),
    };
    let body = match outcome {
        Ok(()) => Body::DesktopAgentFocused(DesktopAgentFocused {}),
        Err(error) => Body::Error(error.into_wire()),
    };
    writer
        .envelope(&session.envelope(Some(id.to_string()), body))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn routed_focus_carries_the_requested_daemon_epoch_to_desktop() -> Result<()> {
        let host = Uuid::new_v4();
        let controller = Uuid::new_v4();
        let run = PresentedRunKey {
            host_id: host.to_string(),
            remote_epoch: Uuid::new_v4().to_string(),
            agent_run_id: Uuid::new_v4().to_string(),
        };
        let (client, server) = tokio::io::duplex(16384);
        let (send, mut received) = mpsc::channel(1);
        let listen = tokio::spawn(exchange(
            Box::new(server),
            host,
            controller,
            controller,
            true,
            send,
        ));
        let request = request_on_stream(Box::new(client), controller, run.clone(), true, async {});
        let confirm = async {
            let request = received.recv().await.expect("missing routed request");
            assert_eq!(request.run, run);
            request
                .result
                .send(Err(ProtocolFault::new(
                    "EpochMismatch",
                    "Daemon restarted.",
                )
                .into()))
                .unwrap();
        };
        let (result, _) = timeout(COMMAND_TIMEOUT, async {
            corvid_async::join!(request, confirm)
        })
        .await?;
        let fault = result.unwrap_err().downcast::<ProtocolFault>()?;
        assert_eq!(fault.code, "EpochMismatch");
        listen.await??;
        Ok(())
    }

    #[tokio::test]
    async fn focus_routes_only_the_exact_host_epoch_and_run() -> Result<()> {
        let host = Uuid::new_v4();
        let controller = Uuid::new_v4();
        let epoch = Uuid::new_v4();
        let run = PresentedRunKey {
            host_id: host.to_string(),
            remote_epoch: epoch.to_string(),
            agent_run_id: Uuid::new_v4().to_string(),
        };
        let (client, server) = tokio::io::duplex(16384);
        let (send, mut received) = mpsc::channel(1);
        let listen = tokio::spawn(exchange(
            Box::new(server),
            host,
            controller,
            epoch,
            false,
            send,
        ));
        let request = request_on_stream(Box::new(client), controller, run.clone(), false, async {});
        let confirm = async {
            let request = received.recv().await.context("missing focus request")?;
            assert_eq!(request.run, run);
            request.result.send(Ok(())).unwrap();
            Ok::<_, anyhow::Error>(())
        };
        timeout(COMMAND_TIMEOUT, async {
            corvid_async::try_join!(request, confirm)
        })
        .await??;
        listen.await??;

        let (client, server) = tokio::io::duplex(16384);
        let (send, mut received) = mpsc::channel(1);
        let listen = tokio::spawn(exchange(
            Box::new(server),
            host,
            controller,
            epoch,
            false,
            send,
        ));
        let mismatched = PresentedRunKey {
            host_id: Uuid::new_v4().to_string(),
            ..run
        };
        let error = timeout(
            COMMAND_TIMEOUT,
            request_on_stream(Box::new(client), controller, mismatched, false, async {}),
        )
        .await?
        .expect_err("another host's identically keyed agent must not focus");
        assert!(error.to_string().contains("different daemon session"));
        assert!(listen.await?.is_err());
        assert!(received.try_recv().is_err());
        Ok(())
    }
}
