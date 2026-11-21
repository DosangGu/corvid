#![cfg(feature = "tokio")]
#![cfg(all(
    feature = "compio",
    any(target_os = "linux", target_os = "macos", windows)
))]

use anyhow::{Context, Result};
use compio::io::{AsyncRead, AsyncWrite};
use corvid_async::oneshot;
use corvid_protocol::wire::{Ping, Pong, envelope::Body};
use corvid_protocol::{
    COMMAND_TIMEOUT, CompioFrameReader, CompioFrameWriter, FrameReader, FrameRole, FrameWriter,
    PeerRole, accept_hello, check_hello_result, hello,
};
use tokio::io::{AsyncRead as TokioRead, AsyncWrite as TokioWrite};

async fn server(read: impl AsyncRead, write: impl AsyncWrite) -> Result<()> {
    let role = FrameRole::StructuredOnly;
    let mut writer = CompioFrameWriter::start(write, role).await?;
    let mut reader = CompioFrameReader::start(read, role).await?;
    let greeting = reader.next_envelope().await?.context("missing Hello")?;
    let (session, reply) =
        accept_hello(&greeting, PeerRole::Control, "host", "epoch", &["ping.v1"])?;
    writer.envelope(&reply).await?;
    let request = reader.next_envelope().await?.context("missing Ping")?;
    anyhow::ensure!(matches!(session.validate_request(&request)?, Body::Ping(_)));
    writer
        .envelope(&session.envelope(request.request_id, Body::Pong(Pong {})))
        .await?;
    Ok(())
}

async fn client(read: impl TokioRead + Unpin, write: impl TokioWrite + Unpin) -> Result<()> {
    let role = FrameRole::StructuredOnly;
    let mut writer = FrameWriter::start(write, role).await?;
    let mut reader = FrameReader::start(read, role).await?;
    let greeting = hello(
        "hello".into(),
        Some("controller".into()),
        vec!["ping.v1".into()],
    );
    writer.envelope(&greeting).await?;
    let reply = reader
        .next_envelope()
        .await?
        .context("missing HelloResult")?;
    let session = check_hello_result(&greeting, &reply, PeerRole::Control)?;
    let request = session.envelope(Some("ping".into()), Body::Ping(Ping {}));
    writer.envelope(&request).await?;
    let response = reader.next_envelope().await?.context("missing Pong")?;
    session.validate_scope(&response)?;
    assert_eq!(response.request_id, request.request_id);
    assert!(matches!(response.body, Some(Body::Pong(_))));
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn accepts_tokio_clients_on_a_private_unix_socket() -> Result<()> {
    use compio::io::util::Splittable;
    use corvid_protocol::unix::{RuntimeDir, SocketRole};

    let fixture = tempfile::tempdir_in(std::env::home_dir().context("home directory")?)?;
    let path = fixture.path().join("runtime");
    let (listening, ready) = oneshot::channel();
    let server_path = path.clone();
    let runner = tokio::task::spawn_blocking(move || -> Result<()> {
        compio::runtime::Runtime::new()?.block_on(compio::time::timeout(
            COMMAND_TIMEOUT,
            async {
                let runtime = RuntimeDir::create(&server_path)?;
                let instance = runtime.lock_instance()?;
                let collector = instance.bind_compio(SocketRole::Collector).await?;
                assert!(instance.bind_compio(SocketRole::Collector).await.is_err());
                drop(collector);
                let listener = instance.bind_compio(SocketRole::Control).await?;
                listening.send(()).ok();
                for _ in 0..2 {
                    let stream = listener
                        .accept()
                        .await?
                        .context("peer authentication failed")?;
                    let (read, write) = Splittable::split(stream);
                    server(read, write).await?;
                }
                Ok::<_, anyhow::Error>(())
            },
        ))?
    });

    tokio::time::timeout(COMMAND_TIMEOUT, ready).await??;
    let runtime = RuntimeDir::open(&path)?;
    for _ in 0..2 {
        let (read, write) = runtime.connect(SocketRole::Control).await?.into_split();
        tokio::time::timeout(COMMAND_TIMEOUT, client(read, write)).await??;
    }
    runner.await??;
    assert!(!path.join("control.sock").exists());
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn accepts_tokio_clients_on_a_private_named_pipe() -> Result<()> {
    use corvid_protocol::windows::{DaemonPipe, StateDirectory};

    let fixture = tempfile::tempdir()?;
    let path = fixture.path().join("runtime");
    let (listening, ready) = oneshot::channel();
    let server_path = path.clone();
    let runner = tokio::task::spawn_blocking(move || -> Result<()> {
        compio::runtime::Runtime::new()?.block_on(compio::time::timeout(
            COMMAND_TIMEOUT,
            async {
                let runtime = StateDirectory::create_all(&server_path)?;
                let mut listener = DaemonPipe::Control.bind_compio(&runtime)?;
                assert!(DaemonPipe::Control.bind_compio(&runtime).is_err());
                listening.send(()).ok();
                for _ in 0..2 {
                    let stream = listener.accept().await?;
                    server(&stream, &stream).await?;
                }
                Ok::<_, anyhow::Error>(())
            },
        ))?
    });

    tokio::time::timeout(COMMAND_TIMEOUT, ready).await??;
    for _ in 0..2 {
        let (read, write) = tokio::io::split(DaemonPipe::Control.connect(&path).await?);
        tokio::time::timeout(COMMAND_TIMEOUT, client(read, write)).await??;
    }
    runner.await??;
    Ok(())
}
