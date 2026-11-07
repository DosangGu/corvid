use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use corvid_protocol::unix::{RuntimeDir, SocketRole};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{GetHost, HostInfo, Ping};
use corvid_protocol::{
    COMMAND_TIMEOUT, FrameReader, FrameRole, FrameWriter, PeerRole, ProtocolFault,
    check_hello_result, hello,
};
use tokio::net::UnixStream;
use tokio::time::timeout;
use uuid::Uuid;

pub async fn host(runtime: &Path) -> Result<HostInfo> {
    match request(runtime, Body::GetHost(GetHost {}), "host.v1").await? {
        Body::HostInfo(host) => Ok(host),
        _ => bail!("unexpected response to GetHost"),
    }
}

pub async fn ping(runtime: &Path) -> Result<()> {
    match request(runtime, Body::Ping(Ping {}), "ping.v1").await? {
        Body::Pong(_) => Ok(()),
        _ => bail!("unexpected response to Ping"),
    }
}

async fn request(runtime: &Path, request: Body, capability: &str) -> Result<Body> {
    let stream = connect(runtime).await?;
    timeout(COMMAND_TIMEOUT, exchange(stream, request, capability))
        .await
        .context("control command timed out")?
}

pub(crate) async fn connect(path: &Path) -> Result<UnixStream> {
    let runtime = RuntimeDir::open(path).context("DaemonUnavailable")?;
    runtime
        .connect(SocketRole::Control)
        .await
        .context("DaemonUnavailable")
}

async fn exchange(stream: UnixStream, body: Body, capability: &str) -> Result<Body> {
    let (read, write) = stream.into_split();
    let mut writer = FrameWriter::start(write, FrameRole::StructuredOnly).await?;
    let mut reader = FrameReader::start(read, FrameRole::StructuredOnly).await?;
    let greeting = hello(
        Uuid::new_v4().to_string(),
        Some(Uuid::new_v4().to_string()),
        vec![capability.to_owned()],
    );
    writer.envelope(&greeting).await?;
    let reply = reader
        .next_envelope()
        .await?
        .context("daemon closed during Hello")?;
    let session = check_hello_result(&greeting, &reply, PeerRole::Control)?;

    let request = session.envelope(Some(Uuid::new_v4().to_string()), body);
    writer.envelope(&request).await?;
    let reply = reader
        .next_envelope()
        .await?
        .context("daemon closed during command")?;
    session.validate_scope(&reply)?;
    ensure!(
        request.request_id == reply.request_id,
        "control request ID mismatch"
    );
    match reply.body.context("unknown response body")? {
        Body::Error(error) => Err(ProtocolFault::from_wire(&error).into()),
        Body::HostInfo(host)
            if host.host_id != session.host_id || host.server_epoch != session.server_epoch =>
        {
            bail!("host response contradicts negotiated daemon identity")
        }
        body => Ok(body),
    }
}
