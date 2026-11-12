use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use corvid_core::Viewport;
#[cfg(target_os = "linux")]
use corvid_protocol::unix::{RuntimeDir, SocketRole};
#[cfg(windows)]
use corvid_protocol::windows::{DaemonPipe, StateDirectory};
use corvid_protocol::{COMMAND_TIMEOUT, RENDER_LIMIT};
use portable_pty::{CommandBuilder, ExitStatus};
use prost::{Enumeration, Message};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::runtime::Handle;
use tokio::time::{Instant, sleep, timeout};
use uuid::Uuid;
use vt100::Screen;

use super::input::{Kind, WriteFailure};
use super::session::{ReadState, TerminalSession};

pub const COMMAND: &str = "terminal-host";
const TICK: Duration = Duration::from_millis(8);
const VERSION: u32 = 1;

#[cfg(target_os = "linux")]
type Connection = tokio::net::UnixStream;
#[cfg(windows)]
type Connection = tokio::net::windows::named_pipe::NamedPipeClient;

#[derive(Clone, Copy, Debug, Enumeration, PartialEq, Eq)]
#[repr(i32)]
pub(super) enum Operation {
    Hello = 0,
    Poll = 1,
    Resize = 2,
    Keys = 3,
    Paste = 4,
    Fence = 5,
    Preview = 6,
    Render = 7,
    Retain = 8,
    CancelRetain = 9,
    Stop = 10,
    History = 11,
}

#[derive(Clone, Message, PartialEq)]
struct Request {
    #[prost(enumeration = "Operation", tag = "1")]
    operation: i32,
    #[prost(uint32, tag = "2")]
    rows: u32,
    #[prost(uint32, tag = "3")]
    cols: u32,
    #[prost(bytes = "vec", tag = "4")]
    bytes: Vec<u8>,
    #[prost(bool, tag = "5")]
    incremental: bool,
    #[prost(uint32, tag = "6")]
    footer_row: u32,
    #[prost(uint32, tag = "7")]
    version: u32,
}

#[derive(Message)]
pub(super) struct Response {
    #[prost(bool, tag = "1")]
    pub(super) rejected: bool,
    #[prost(bool, tag = "2")]
    pub(super) changed: bool,
    #[prost(bool, tag = "3")]
    pub(super) paste_pending: bool,
    #[prost(uint32, optional, tag = "4")]
    pub(super) exit_code: Option<u32>,
    #[prost(string, tag = "5")]
    pub(super) signal: String,
    #[prost(uint32, optional, tag = "6")]
    pub(super) pid: Option<u32>,
    #[prost(uint32, tag = "7")]
    rows: u32,
    #[prost(uint32, tag = "8")]
    cols: u32,
    #[prost(bytes = "vec", tag = "9")]
    bytes: Vec<u8>,
    #[prost(uint32, tag = "10")]
    version: u32,
    #[prost(uint32, tag = "11")]
    failure: u32,
    #[prost(bool, tag = "12")]
    partial: bool,
    #[prost(message, optional, tag = "13")]
    history: Option<corvid_protocol::wire::TerminalHistory>,
    #[cfg(windows)]
    #[prost(uint64, optional, tag = "14")]
    process_created: Option<u64>,
    #[prost(string, tag = "15")]
    pub(super) title: String,
}

impl Response {
    pub(super) fn status(&self) -> Option<ExitStatus> {
        self.exit_code.map(|code| {
            if self.signal.is_empty() {
                ExitStatus::with_exit_code(code)
            } else {
                ExitStatus::with_signal(&self.signal)
            }
        })
    }

    pub(super) fn write_failure(&self) -> Option<WriteFailure> {
        let kind = match self.failure {
            1 => Kind::Keys,
            2 => Kind::Paste,
            3 => Kind::Reply,
            _ => return None,
        };
        Some(WriteFailure {
            kind,
            partial: self.partial,
        })
    }
}

pub(super) struct Client {
    connection: Connection,
    executor: Handle,
    pub(super) pid: Option<u32>,
    #[cfg(windows)]
    pub(super) process_key: Option<crate::native_process::ProcessKey>,
    pub(super) paste_pending: bool,
    pub(super) retained: bool,
}

#[derive(Clone)]
pub(crate) struct HostConfig {
    pub(crate) root: PathBuf,
    executable: PathBuf,
}

impl HostConfig {
    pub(crate) fn new(root: &Path) -> Result<Self> {
        let executable = std::env::current_exe()?;
        #[cfg(windows)]
        let executable = {
            let runtime = StateDirectory::open(root)?;
            let name = format!("pty-host-{}.exe", Uuid::new_v4().simple());
            for entry in std::fs::read_dir(root)? {
                let entry = entry?;
                if entry.file_name().to_str().is_some_and(|name| {
                    name.strip_prefix("pty-host-")
                        .and_then(|name| name.strip_suffix(".exe"))
                        .is_some_and(|id| id.len() == 32 && Uuid::parse_str(id).is_ok())
                }) && entry.file_type()?.is_file()
                {
                    // Images still mapped by a host remain available across the upgrade.
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            runtime.replace_private_file(&name, &std::fs::read(&executable)?)?;
            root.join(name)
        };
        Ok(Self {
            root: root.into(),
            executable,
        })
    }
}

impl Client {
    pub(super) fn history(&mut self) -> Result<corvid_protocol::wire::TerminalHistory> {
        let response = self.command(Operation::History, Vec::new())?;
        ensure!(!response.rejected, "terminal host could not read history");
        response
            .history
            .context("terminal host did not return history")
    }
    pub(super) fn start(
        config: &HostConfig,
        id: Uuid,
        command: CommandBuilder,
        size: Viewport,
    ) -> Result<Self> {
        let directory = directory(&config.root, id);
        let mut helper = Command::new(&config.executable);
        helper
            .arg(COMMAND)
            .arg(&directory)
            .arg(id.to_string())
            .arg(size.cols.to_string())
            .arg(size.rows.to_string())
            .arg(command.get_cwd().map_or_else(OsString::new, Clone::clone))
            .args(command.get_argv())
            .env_clear()
            .envs(helper_environment(&command))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;

            // The host keeps its own PTY and can outlive a planned daemon shutdown.
            unsafe {
                helper.pre_exec(|| {
                    if nix::libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
            helper.creation_flags(0x0000_0008 | 0x0000_0200);
        }
        let mut child = helper.spawn().context("starting terminal host")?;
        let result = Self::connect(&config.root, id, true);
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        } else {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        result
    }

    pub(super) fn connect(root: &Path, id: Uuid, starting: bool) -> Result<Self> {
        let directory = directory(root, id);
        let executor = Handle::current();
        let connection = executor.block_on(timeout(COMMAND_TIMEOUT, async {
            loop {
                let result = open(&directory).await;
                if result.is_ok() || !starting {
                    return result;
                }
                sleep(TICK).await;
            }
        }))??;
        let mut client = Self {
            connection,
            executor,
            pid: None,
            #[cfg(windows)]
            process_key: None,
            paste_pending: false,
            retained: false,
        };
        let response = client.call(Request {
            operation: Operation::Hello as i32,
            bytes: id.as_bytes().to_vec(),
            version: VERSION,
            ..Default::default()
        })?;
        ensure!(
            response.version == VERSION && !response.rejected,
            "incompatible terminal host"
        );
        client.pid = response.pid;
        #[cfg(windows)]
        if let Some((pid, created)) = response.pid.zip(response.process_created) {
            client.process_key = Some(crate::native_process::ProcessKey { pid, created });
        }
        Ok(client)
    }

    pub(super) fn command(&mut self, operation: Operation, bytes: Vec<u8>) -> Result<Response> {
        let response = self.call(Request {
            operation: operation as i32,
            bytes,
            ..Default::default()
        })?;
        if !response.rejected {
            match operation {
                Operation::Retain => self.retained = true,
                Operation::CancelRetain => self.retained = false,
                _ => {}
            }
        }
        Ok(response)
    }

    pub(super) fn resize(&mut self, viewport: Viewport) -> Result<()> {
        let response = self.call(Request {
            operation: Operation::Resize as i32,
            rows: viewport.rows.get().into(),
            cols: viewport.cols.get().into(),
            ..Default::default()
        })?;
        ensure!(!response.rejected, "terminal host could not resize the PTY");
        Ok(())
    }

    pub(super) fn render(
        &mut self,
        previous: bool,
        outer: Viewport,
        footer: Option<(u16, &str)>,
    ) -> Result<String> {
        let response = self.call(Request {
            operation: Operation::Render as i32,
            rows: outer.rows.get().into(),
            cols: outer.cols.get().into(),
            incremental: previous,
            footer_row: footer.map_or(0, |(row, _)| row.into()),
            bytes: footer.map_or_else(Vec::new, |(_, ansi)| ansi.as_bytes().to_vec()),
            ..Default::default()
        })?;
        ensure!(!response.rejected, "terminal host could not render");
        Ok(String::from_utf8(response.bytes)?)
    }

    pub(super) fn preview(&mut self) -> Result<(Viewport, Vec<u8>)> {
        let response = self.command(Operation::Preview, Vec::new())?;
        ensure!(
            !response.rejected,
            "terminal host could not render a preview"
        );
        Ok((viewport(response.cols, response.rows)?, response.bytes))
    }

    fn call(&mut self, request: Request) -> Result<Response> {
        let response: Response = self.executor.block_on(timeout(COMMAND_TIMEOUT, async {
            write_message(&mut self.connection, request).await?;
            read_message(&mut self.connection).await
        }))??;
        self.paste_pending = response.paste_pending;
        Ok(response)
    }
}

fn helper_environment(command: &CommandBuilder) -> Vec<(OsString, OsString)> {
    let mut keys: std::collections::BTreeSet<OsString> =
        std::env::vars_os().map(|(key, _)| key).collect();
    keys.extend(command.iter_extra_env_as_str().map(|(key, _)| key.into()));
    keys.extend(
        [
            "TERM",
            "COLORTERM",
            "PATH",
            "CORVID_SHELL_COMMANDS",
            "CORVID_ORIGINAL_ZDOTDIR",
            "ZDOTDIR",
            corvid_protocol::collector::HOST_ID,
            corvid_protocol::collector::SERVER_EPOCH,
            corvid_protocol::collector::LAUNCH_ASSOCIATION,
            corvid_protocol::collector::SHELL_TERMINAL,
            corvid_protocol::collector::RUNTIME_DIR,
        ]
        .into_iter()
        .map(OsString::from),
    );
    keys.into_iter()
        .filter_map(|key| {
            command
                .get_env(&key)
                .map(|value| (key.clone(), value.to_owned()))
        })
        .collect()
}

fn directory(root: &Path, id: Uuid) -> PathBuf {
    root.join(format!("p-{:012x}", id.as_u128() >> 80))
}

#[cfg(target_os = "linux")]
async fn open(path: &Path) -> Result<Connection> {
    RuntimeDir::open(path)?
        .connect(SocketRole::TerminalHost)
        .await
}

#[cfg(windows)]
async fn open(path: &Path) -> Result<Connection> {
    DaemonPipe::TerminalHost.connect(path).await
}

pub fn run_helper(mut args: impl Iterator<Item = OsString>) -> Result<()> {
    let path = PathBuf::from(args.next().context("missing terminal host directory")?);
    let id: Uuid = numeric_arg(&mut args)?;
    let cols = numeric_arg(&mut args)?;
    let rows = numeric_arg(&mut args)?;
    let size = viewport(cols, rows)?;
    let cwd = args.next().context("missing terminal working directory")?;
    let mut command = if let Some(program) = args.next() {
        let mut command = CommandBuilder::new(program);
        command.args(args);
        command
    } else {
        CommandBuilder::new_default_prog()
    };
    if !cwd.is_empty() {
        command.cwd(cwd);
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(serve(&path, id, command, size))
}

fn numeric_arg<T: std::str::FromStr>(args: &mut impl Iterator<Item = OsString>) -> Result<T> {
    args.next()
        .and_then(|arg| arg.to_str()?.parse().ok())
        .context("invalid terminal host argument")
}

#[cfg(target_os = "linux")]
async fn serve(path: &Path, id: Uuid, command: CommandBuilder, size: Viewport) -> Result<()> {
    let runtime = RuntimeDir::create(path)?;
    let instance = runtime.lock_instance()?;
    let listener = instance.bind(SocketRole::TerminalHost).await?;
    let mut host = Host::new(id, command, size)?;
    loop {
        let stream = host
            .wait(async {
                loop {
                    if let Some(stream) = listener.accept().await? {
                        return Ok(stream);
                    }
                }
            })
            .await?;
        match host.attached(stream).await {
            Ok(true) => {}
            Ok(false) => break,
            Err(_) if host.retain => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(windows)]
async fn serve(path: &Path, id: Uuid, command: CommandBuilder, size: Viewport) -> Result<()> {
    let runtime = StateDirectory::create_all(path)?;
    let _instance = runtime.lock_instance()?;
    let mut listener = DaemonPipe::TerminalHost.bind(&runtime)?;
    let mut host = Host::new(id, command, size)?;
    loop {
        let stream = host.wait(listener.accept()).await?;
        match host.attached(stream).await {
            Ok(true) => {}
            Ok(false) => break,
            Err(_) if host.retain => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

struct Host {
    id: Uuid,
    session: TerminalSession,
    previous: Option<Screen>,
    exit: Option<ExitStatus>,
    failure: Option<WriteFailure>,
    changed: bool,
    retain: bool,
    deadline: Instant,
}

impl Host {
    fn new(id: Uuid, command: CommandBuilder, size: Viewport) -> Result<Self> {
        Ok(Self {
            id,
            session: TerminalSession::start(command, size)?,
            previous: None,
            exit: None,
            failure: None,
            changed: true,
            retain: false,
            deadline: Instant::now() + COMMAND_TIMEOUT,
        })
    }

    async fn wait<S>(&mut self, accept: impl Future<Output = Result<S>>) -> Result<S> {
        let mut interval = tokio::time::interval(TICK);
        let deadline = if self.retain {
            None
        } else {
            Some(self.deadline)
        };
        let accept = async {
            match deadline {
                Some(deadline) => tokio::time::timeout_at(deadline, accept).await?,
                None => accept.await,
            }
        };
        tokio::pin!(accept);
        loop {
            tokio::select! {
                result = &mut accept => return result,
                _ = interval.tick() => {
                    self.advance()?;
                    ensure!(self.exit.is_none(), "terminal exited during handoff");
                }
            }
        }
    }

    async fn attached(&mut self, mut stream: impl AsyncRead + AsyncWrite + Unpin) -> Result<bool> {
        let greeting: Request = timeout(COMMAND_TIMEOUT, read_message(&mut stream)).await??;
        if !(greeting.operation == Operation::Hello as i32
            && greeting.version == VERSION
            && greeting.bytes == self.id.as_bytes())
        {
            let rejection = Response {
                rejected: true,
                version: VERSION,
                ..Default::default()
            };
            timeout(COMMAND_TIMEOUT, write_message(&mut stream, rejection)).await??;
            bail!("terminal host identity or version mismatch");
        }
        self.previous = None;
        self.retain = false;
        let mut response = self.response();
        response.version = VERSION;
        #[cfg(target_os = "linux")]
        {
            response.pid = self.session.pid();
        }
        #[cfg(windows)]
        if let Some(process) = self.session.process_key() {
            response.pid = Some(process.pid);
            response.process_created = Some(process.created);
        }
        timeout(COMMAND_TIMEOUT, write_message(&mut stream, response)).await??;
        let mut interval = tokio::time::interval(TICK);
        loop {
            let request = {
                let read = read_message(&mut stream);
                tokio::pin!(read);
                loop {
                    tokio::select! {
                        result = &mut read => break result,
                        _ = interval.tick() => self.advance()?,
                    }
                }
            };
            let Ok(request) = request else { break };
            let response = self.handle(request);
            if !matches!(
                timeout(COMMAND_TIMEOUT, write_message(&mut stream, response)).await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
        if self.retain {
            self.session.fence_input()?;
        }
        Ok(self.retain)
    }

    fn advance(&mut self) -> Result<()> {
        if self.exit.is_none() {
            let (state, output) = self.session.read_output()?;
            self.changed |= output;
            if !matches!(state, ReadState::BudgetUsed) {
                self.exit = self.session.try_wait()?;
            }
            if self.failure.is_none() {
                self.failure = if self.exit.is_some() {
                    self.session.input.interrupted_paste()
                } else if matches!(state, ReadState::Eof) {
                    None
                } else {
                    self.session.flush_input().err()
                };
            }
        }
        Ok(())
    }

    fn response(&self) -> Response {
        Response {
            title: self.session.screen.title().to_owned(),
            paste_pending: self.session.input.paste_pending(),
            exit_code: self.exit.as_ref().map(ExitStatus::exit_code),
            signal: self
                .exit
                .as_ref()
                .and_then(ExitStatus::signal)
                .unwrap_or_default()
                .into(),
            ..Default::default()
        }
    }

    fn handle(&mut self, request: Request) -> Response {
        let mut response = self.response();
        response.rejected = self.apply(request, &mut response).is_err();
        response.paste_pending = self.session.input.paste_pending();
        if response.encoded_len() > RENDER_LIMIT {
            response.bytes.clear();
            response.rejected = true;
        }
        response
    }

    fn apply(&mut self, request: Request, response: &mut Response) -> Result<()> {
        match Operation::try_from(request.operation)? {
            Operation::Hello => bail!("terminal host is already connected"),
            Operation::Poll => {
                response.changed = std::mem::take(&mut self.changed);
                if let Some(failure) = self.failure.take() {
                    response.failure = match failure.kind {
                        Kind::Keys => 1,
                        Kind::Paste => 2,
                        Kind::Reply => 3,
                    };
                    response.partial = failure.partial;
                    self.session.fence_input()?;
                }
            }
            Operation::Resize => {
                ensure!(!self.session.input.paste_pending(), "paste is pending");
                self.session.complete_input_write()?;
                self.session.resize(viewport(request.cols, request.rows)?)?;
                self.session.input.fence();
            }
            Operation::Keys => ensure!(
                self.session.input.user(request.bytes),
                "terminal input queue is full"
            ),
            Operation::Paste => ensure!(
                self.session
                    .input
                    .paste(request.bytes, self.session.screen.bracketed_paste()),
                "terminal paste queue is full"
            ),
            Operation::Fence => self.session.fence_input()?,
            Operation::Preview => {
                let size = self.session.screen.viewport();
                response.cols = size.cols.get().into();
                response.rows = size.rows.get().into();
                response.bytes = self.session.screen.redraw(size)?.into_bytes();
            }
            Operation::History => response.history = Some(self.session.screen.history()),
            Operation::Render => {
                let size = viewport(request.cols, request.rows)?;
                let footer = if request.footer_row == 0 {
                    None
                } else {
                    Some((
                        request.footer_row.try_into()?,
                        std::str::from_utf8(&request.bytes)?,
                    ))
                };
                let previous = self.previous.as_ref().filter(|_| request.incremental);
                response.bytes = self
                    .session
                    .screen
                    .render(previous, size, footer)?
                    .into_bytes();
                self.previous = Some(self.session.screen.snapshot());
            }
            Operation::Retain => {
                ensure!(!self.session.input.paste_pending(), "paste is pending");
                self.session.fence_input()?;
                self.retain = true;
            }
            Operation::CancelRetain => self.retain = false,
            Operation::Stop => {
                self.exit = self.session.stop();
                *response = self.response();
            }
        }
        Ok(())
    }
}

fn viewport(cols: u32, rows: u32) -> Result<Viewport> {
    Viewport::new(cols.try_into()?, rows.try_into()?).context("invalid terminal host viewport")
}

async fn read_message<M: Message + Default>(reader: &mut (impl AsyncRead + Unpin)) -> Result<M> {
    let size = reader.read_u32().await? as usize;
    ensure!(size <= RENDER_LIMIT, "terminal host frame exceeds limit");
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes).await?;
    Ok(M::decode(bytes.as_slice())?)
}

async fn write_message(
    writer: &mut (impl AsyncWrite + Unpin),
    message: impl Message,
) -> Result<()> {
    let bytes = message.encode_to_vec();
    ensure!(
        bytes.len() <= RENDER_LIMIT,
        "terminal host frame exceeds limit"
    );
    writer.write_u32(bytes.len().try_into()?).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_oversized_frames_before_reading_the_payload() {
        let (mut writer, mut reader) = tokio::io::duplex(4);
        writer.write_u32((RENDER_LIMIT + 1) as u32).await.unwrap();
        let result = read_message::<Request>(&mut reader).await;
        assert!(result.unwrap_err().to_string().contains("exceeds limit"));
    }

    #[tokio::test]
    async fn round_trips_the_versioned_terminal_host_request() {
        let (mut writer, mut reader) = tokio::io::duplex(256);
        let request = Request {
            operation: Operation::Hello as i32,
            bytes: Uuid::new_v4().as_bytes().to_vec(),
            version: VERSION,
            ..Default::default()
        };
        write_message(&mut writer, request.clone()).await.unwrap();
        let decoded: Request = read_message(&mut reader).await.unwrap();
        assert_eq!(decoded, request);
    }
}
