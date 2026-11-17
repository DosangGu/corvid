use anyhow::{Context, Result, ensure};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{DesktopAgentFocused, FocusDesktopAgent, PresentedRunKey};
use corvid_protocol::{
    COMMAND_TIMEOUT, PeerRole, ProtocolFault, accept_hello, check_hello_result, hello,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::timeout;
use uuid::Uuid;

use crate::connection::{self, Stream};
use crate::desktop::FocusRequest;

const CAPABILITY: &str = "desktop_focus.v1";
#[cfg(windows)]
const PIPE_PREFIX: &str = "corvid-desktop-focus-v1";
const MAX_PEERS: usize = 4;

/// Request exact navigation from an already attached Desktop on this host.
pub async fn request(controller: Uuid, run: PresentedRunKey) -> Result<()> {
    timeout(COMMAND_TIMEOUT, async {
        let stream = connect(controller)
            .await
            .context("Open this host in Corvid Desktop to focus its agents.")?;
        request_on_stream(stream, controller, run).await
    })
    .await
    .context("Desktop did not confirm focus in time")?
}

async fn request_on_stream(stream: Stream, controller: Uuid, run: PresentedRunKey) -> Result<()> {
    let (mut reader, mut writer) = connection::frames(stream).await?;
    let greeting = hello(
        Uuid::new_v4().to_string(),
        Some(controller.to_string()),
        vec![CAPABILITY.into()],
    );
    writer.envelope(&greeting).await?;
    let reply = reader
        .next_envelope()
        .await?
        .context("Desktop closed during Hello")?;
    let session = check_hello_result(&greeting, &reply, PeerRole::Presentation)?;
    ensure!(
        session.host_id == run.host_id && session.server_epoch == run.remote_epoch,
        "The agent belongs to a different daemon session."
    );
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
async fn connect(controller: Uuid) -> Result<Stream> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

    let pipe = corvid_protocol::windows::open_scoped(PIPE_PREFIX, &controller.simple().to_string())
        .await?;
    let mut pid = 0;
    ensure!(
        unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } != 0 && pid != 0,
        "could not identify the Desktop process"
    );
    // On a click, Deck can pass its foreground permission to the private
    // endpoint's actual owner. The Desktop UI reports any refusal to activate.
    unsafe { AllowSetForegroundWindow(pid) };
    Ok(Box::new(pipe))
}

#[cfg(not(windows))]
async fn connect(controller: Uuid) -> Result<Stream> {
    use corvid_protocol::unix::{RuntimeDir, SocketRole};

    let path = corvid_protocol::paths::desktop_focus_socket(controller)?.prepare()?;
    Ok(Box::new(
        RuntimeDir::open(&path)?
            .connect(SocketRole::Presentation)
            .await?,
    ))
}

/// This listener is owned by an already attached Desktop session.
pub async fn serve(
    host: Uuid,
    controller: Uuid,
    epoch: Uuid,
    focus: mpsc::Sender<FocusRequest>,
) -> Result<()> {
    #[cfg(windows)]
    let mut listener = corvid_protocol::windows::PipeListener::bind_scoped(
        PIPE_PREFIX,
        &controller.simple().to_string(),
    )?;
    #[cfg(not(windows))]
    use corvid_protocol::unix::{RuntimeDir, SocketRole};
    #[cfg(not(windows))]
    let directory = RuntimeDir::create_all(
        &corvid_protocol::paths::desktop_focus_socket(controller)?.prepare()?,
    )?;
    #[cfg(not(windows))]
    let instance = directory.lock_instance()?;
    #[cfg(not(windows))]
    let listener = instance.bind(SocketRole::Presentation).await?;
    let mut peers = JoinSet::new();
    loop {
        tokio::select! {
            _ = focus.closed() => return Ok(()),
            _ = peers.join_next(), if !peers.is_empty() => {}
            connected = listener.accept(), if peers.len() < MAX_PEERS => {
                #[cfg(windows)]
                let stream = connected?;
                #[cfg(not(windows))]
                let Some(stream) = connected? else { continue };
                let focus = focus.clone();
                peers.spawn(async move {
                    let _ = timeout(COMMAND_TIMEOUT, exchange(
                        Box::new(stream), host, controller, epoch, focus,
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
    focus: mpsc::Sender<FocusRequest>,
) -> Result<()> {
    let (mut reader, mut writer) = connection::frames(stream).await?;
    let greeting = reader
        .next_envelope()
        .await?
        .context("missing Desktop focus Hello")?;
    let (session, reply) = accept_hello(
        &greeting,
        PeerRole::Presentation,
        &host.to_string(),
        &epoch.to_string(),
        &[CAPABILITY],
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
    let id = connection::request_id(&request)?;
    let Some(Body::FocusDesktopAgent(body)) = request.body else {
        anyhow::bail!("unsupported Desktop focus request");
    };
    let run = body.run.context("missing agent identity")?;
    ensure!(
        run.host_id == session.host_id
            && run.remote_epoch == session.server_epoch
            && Uuid::parse_str(&run.agent_run_id).is_ok(),
        "invalid Desktop focus scope"
    );
    let (result, completed) = oneshot::channel();
    let outcome = match focus.try_send(FocusRequest { run, result }) {
        Ok(()) => completed
            .await
            .context("Desktop focus result unavailable")?,
        Err(_) => Err("Desktop is unavailable or handling another focus request.".into()),
    };
    let body = match outcome {
        Ok(()) => Body::DesktopAgentFocused(DesktopAgentFocused {}),
        Err(error) => Body::Error(ProtocolFault::new("FocusUnavailable", &error).into_wire()),
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
        let listen = tokio::spawn(exchange(Box::new(server), host, controller, epoch, send));
        let request = request_on_stream(Box::new(client), controller, run.clone());
        let confirm = async {
            let request = received.recv().await.context("missing focus request")?;
            assert_eq!(request.run, run);
            request.result.send(Ok(())).unwrap();
            Ok::<_, anyhow::Error>(())
        };
        timeout(COMMAND_TIMEOUT, async {
            tokio::try_join!(request, confirm)
        })
        .await??;
        listen.await??;

        let (client, server) = tokio::io::duplex(16384);
        let (send, mut received) = mpsc::channel(1);
        let listen = tokio::spawn(exchange(Box::new(server), host, controller, epoch, send));
        let mismatched = PresentedRunKey {
            host_id: Uuid::new_v4().to_string(),
            ..run
        };
        let error = timeout(
            COMMAND_TIMEOUT,
            request_on_stream(Box::new(client), controller, mismatched),
        )
        .await?
        .expect_err("another host's identically keyed agent must not focus");
        assert!(error.to_string().contains("different daemon session"));
        assert!(listen.await?.is_err());
        assert!(received.try_recv().is_err());
        Ok(())
    }
}
