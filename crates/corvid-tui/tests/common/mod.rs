use std::path::Path;
use std::time::Duration;

use async_process::{Child, Command};
use corvid_async::time::{sleep, timeout};
use corvid_protocol::unix::{RuntimeDir, SocketRole};

const BINARY: &str = env!("CARGO_BIN_EXE_corvid");
pub const WAIT: Duration = Duration::from_secs(8);

pub fn routing_diagnostics() -> bool {
    std::env::var_os("CORVID_TEST_ROUTING_DIAGNOSTICS").is_some_and(|value| value == "1")
}

#[allow(dead_code)]
pub fn install_routing_panic_hook() {
    static HOOK: std::sync::Once = std::sync::Once::new();

    if !routing_diagnostics() {
        return;
    }

    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current();
            let name = thread.name().unwrap_or("<unnamed>");
            let location = info.location().map_or_else(
                || "<unknown>".to_string(),
                |loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()),
            );
            let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string panic payload>".to_string()
            };
            let backtrace = std::backtrace::Backtrace::capture();
            let message = format!(
                "[routing panic] thread={name} location={location}: {payload}\n{backtrace}\n"
            );
            write_raw_stderr(message.as_bytes());
            previous(info);
        }));
    });
}

#[allow(dead_code)]
#[cfg(unix)]
fn write_raw_stderr(mut bytes: &[u8]) {
    while !bytes.is_empty() {
        // SAFETY: `STDERR_FILENO` is a valid file descriptor and `bytes` is a valid
        // initialized byte slice for the duration of the syscall.
        let written = unsafe {
            nix::libc::write(nix::libc::STDERR_FILENO, bytes.as_ptr().cast(), bytes.len())
        };
        if written > 0 {
            bytes = &bytes[written as usize..];
        } else if written == -1
            && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
        {
            continue;
        } else {
            break;
        }
    }
}

#[allow(dead_code)]
#[cfg(not(unix))]
fn write_raw_stderr(bytes: &[u8]) {
    eprint!("{}", String::from_utf8_lossy(bytes));
}

pub fn command(runtime: &Path) -> Command {
    let mut command = Command::new(BINARY);
    command.arg("--runtime-dir").arg(runtime).kill_on_drop(true);
    command.env("CORVID_VIEW_STATE_DIR", runtime.join("client-views"));
    command
}

pub async fn daemon(runtime: &Path, state: &Path) -> Child {
    start_daemon(command(runtime), runtime, state).await
}

pub async fn start_daemon(mut command: Command, runtime: &Path, state: &Path) -> Child {
    command.arg("daemon").arg("--state-dir").arg(state);
    wait_for_daemon(command.spawn().unwrap(), runtime).await
}

pub async fn wait_for_daemon(mut daemon: Child, runtime: &Path) -> Child {
    let pid = daemon.id();
    if routing_diagnostics() {
        eprintln!("daemon pid={pid} starting, runtime={}", runtime.display());
    }
    timeout(WAIT, async {
        loop {
            if let Some(status) = daemon.try_status().unwrap() {
                panic!("daemon pid={pid} exited during startup: {status}");
            }
            if let Ok(directory) = RuntimeDir::open(runtime)
                && corvid_client::ping(runtime).await.is_ok()
                && directory.connect(SocketRole::Client).await.is_ok()
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|error| panic!("daemon pid={pid} did not become ready: {error}"));
    if routing_diagnostics() {
        eprintln!("daemon pid={pid} ready");
    }
    daemon
}
