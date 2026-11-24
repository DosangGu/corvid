#[cfg(unix)]
use std::process::Stdio;
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use compio::buf::IoBufMut;
use compio::io::AsyncRead;
use compio::runtime::fd::AsyncFd;
use corvid_protocol::{FrameError, ProtocolFault};

#[cfg(unix)]
type Descriptor = std::os::fd::OwnedFd;
#[cfg(windows)]
type Descriptor = std::os::windows::io::OwnedHandle;

pub(super) type Pipe = AsyncFd<Descriptor>;

pub(super) struct Process {
    pub(super) child: async_process::Child,
    pub(super) stdin: Pipe,
    pub(super) stdout: ReadPipe,
    pub(super) stderr: ReadPipe,
}

impl Process {
    #[cfg(unix)]
    pub(super) async fn start(command: Command) -> Result<Self> {
        let mut child = async_process::Command::from(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("starting Corvid relay")?;
        let stdin = child.stdin.take().context("relay stdin is unavailable")?;
        let stdout = child.stdout.take().context("relay stdout is unavailable")?;
        let stderr = child.stderr.take().context("relay stderr is unavailable")?;
        Ok(Self {
            child,
            stdin: AsyncFd::new(stdin.try_into()?)?,
            stdout: ReadPipe(AsyncFd::new(stdout.try_into()?)?),
            stderr: ReadPipe(AsyncFd::new(stderr.try_into()?)?),
        })
    }

    #[cfg(windows)]
    pub(super) async fn start(command: Command) -> Result<Self> {
        use corvid_protocol::windows::child_stdio_pipe;

        let (stdin, child_stdin) = child_stdio_pipe(true).await?;
        let (stdout, child_stdout) = child_stdio_pipe(false).await?;
        let (stderr, child_stderr) = child_stdio_pipe(false).await?;
        let child = async_process::Command::from(command)
            .stdin(child_stdin)
            .stdout(child_stdout)
            .stderr(child_stderr)
            .kill_on_drop(true)
            .spawn()
            .context("starting Corvid relay")?;
        Ok(Self {
            child,
            stdin,
            stdout: ReadPipe(stdout),
            stderr: ReadPipe(stderr),
        })
    }
}

pub(super) struct Output {
    pub(super) status: ExitStatus,
    pub(super) stdout: Vec<u8>,
    pub(super) diagnostic: Vec<u8>,
}

pub(super) async fn output(
    command: Command,
    payload: Option<(Vec<u8>, Vec<u8>)>,
    limit: usize,
    timeout: Duration,
    timeout_context: &str,
) -> Result<Output> {
    use compio::io::AsyncWriteExt;
    use compio::time::timeout_at;

    let deadline = Instant::now() + timeout;
    let Process {
        mut child,
        mut stdin,
        stdout,
        stderr,
    } = timeout_at(deadline, Process::start(command))
        .await
        .with_context(|| format!("{timeout_context} while starting the child and its pipes"))??;

    let mut input = "closing stdin";
    let mut stdout_done = false;
    let mut stderr_done = false;
    let send = async {
        let result = async {
            if let Some((header, binary)) = payload {
                input = "writing the header";
                stdin.write_all(header).await.0?;
                input = "writing the payload";
                stdin.write_all(binary).await.0?;
            }
            Ok::<_, std::io::Error>(())
        }
        .await;
        // Deliver EOF independently of the child wait and output readers. AsyncFd's
        // AsyncWrite::shutdown leaves this pipe open.
        drop(stdin);
        input = if result.is_ok() {
            "stdin closed"
        } else {
            "stdin write failed and pipe closed"
        };
        result
    };
    let receive_stdout = async {
        let result = read_bounded(stdout, limit).await;
        stdout_done = true;
        result
    };
    let receive_stderr = async {
        let result = drain_diagnostic(stderr).await;
        stderr_done = true;
        result
    };
    let result = timeout_at(deadline, async {
        corvid_async::join!(child.status(), receive_stdout, receive_stderr, send)
    })
    .await;
    let (status, received, diagnostic, sent) = result.with_context(|| {
        let status = child.try_status();
        format!(
            "{timeout_context} waiting for the child ({input}, stdout completed: {stdout_done}, stderr completed: {stderr_done}, exit status: {status:?})"
        )
    })?;
    let status = status?;
    let stdout = if status.success() {
        sent?;
        received?
    } else {
        Vec::new()
    };
    Ok(Output {
        status,
        stdout,
        diagnostic,
    })
}

async fn read_bounded(mut pipe: ReadPipe, limit: usize) -> Result<Vec<u8>, FrameError> {
    use compio::io::AsyncReadExt;

    let mut bytes = Vec::with_capacity(limit);
    while bytes.len() < limit {
        let compio::BufResult(result, read) = pipe.append(bytes).await;
        bytes = read;
        match result.map_err(FrameError::from) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(bytes)
}

pub(super) async fn drain_diagnostic(mut pipe: ReadPipe) -> Vec<u8> {
    let mut tail = Vec::with_capacity(super::MAX_DIAGNOSTIC);
    let mut scratch = [0; 1024];
    loop {
        let compio::BufResult(result, data) = pipe.read(scratch).await;
        scratch = data;
        let Ok(count @ 1..) = result else {
            return tail;
        };
        let discard = (tail.len() + count).saturating_sub(super::MAX_DIAGNOSTIC);
        tail.drain(..discard);
        tail.extend_from_slice(&scratch[..count]);
    }
}

pub(super) async fn classify(
    error: anyhow::Error,
    child: &mut async_process::Child,
    diagnostic: &mut compio::runtime::JoinHandle<Vec<u8>>,
) -> anyhow::Error {
    if error.is::<ProtocolFault>() {
        return error;
    }
    let evidence = compio::time::timeout(super::DIAGNOSTIC_TIMEOUT, async {
        if child.status().await.ok()?.success() {
            return None;
        }
        diagnostic.await.ok()
    })
    .await;
    match evidence {
        Ok(Some(bytes)) => super::relay_failure(&bytes).into(),
        _ => error,
    }
}

pub(super) struct ReadPipe(Pipe);

impl AsyncRead for ReadPipe {
    async fn read<B: IoBufMut>(&mut self, bytes: B) -> compio::BufResult<usize, B> {
        let result = self.0.read(bytes).await;
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED};

            // Byte pipes report ordinary EOF as a Windows error. Translate it
            // here so the frame reader still distinguishes a truncated frame.
            if result.0.as_ref().is_err_and(|error| {
                matches!(
                    error.raw_os_error().map(|code| code as u32),
                    Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED)
                )
            }) {
                return compio::BufResult(Ok(0), result.1);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHILD_FIXTURE: &str = "endpoint::process::tests::native_process_child_fixture";
    const CHILD_FIXTURE_MODE: &str = "CORVID_TEST_NATIVE_PROCESS_CHILD";

    #[test]
    #[ignore = "run as a subprocess by the native process test"]
    fn native_process_child_fixture() -> Result<()> {
        use std::io::{Read, Write};

        if !std::env::var_os(CHILD_FIXTURE_MODE).is_some_and(|value| value == "1") {
            return Ok(());
        }

        let mut payload = Vec::new();
        std::io::stdin().read_to_end(&mut payload)?;
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&payload)?;
        stdout.flush()?;
        std::io::stderr().write_all(b"diagnostic")?;
        // Skip libtest's summary after the raw payload.
        std::process::exit(0);
    }

    #[compio::test]
    async fn native_process_transfers_payload_until_eof_and_reaps_the_child() -> Result<()> {
        let mut command = Command::new(std::env::current_exe()?);
        command.env(CHILD_FIXTURE_MODE, "1").args([
            "--exact",
            CHILD_FIXTURE,
            "--ignored",
            "--nocapture",
            "--quiet",
        ]);
        let reply = super::super::native::command_output(
            command,
            Some((b"header ", b"body")),
            64,
            std::time::Duration::from_secs(10),
            "native process fixture timed out",
        )
        .await?;
        assert!(reply.status.success(), "{:?}", reply.diagnostic);
        // Libtest prints its announcement before the fixture writes the payload.
        assert_eq!(
            reply.stdout.rsplit(|&byte| byte == b'\n').next(),
            Some(b"header body".as_slice()),
            "{:?}",
            String::from_utf8_lossy(&reply.stdout),
        );
        assert_eq!(reply.diagnostic, b"diagnostic");
        Ok(())
    }
}
