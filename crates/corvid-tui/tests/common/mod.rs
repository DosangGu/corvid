use std::path::Path;
use std::time::Duration;

use corvid_async::time::{sleep, timeout};
use corvid_protocol::unix::{RuntimeDir, SocketRole};
use tokio::process::{Child, Command};

const BINARY: &str = env!("CARGO_BIN_EXE_corvid");
pub const WAIT: Duration = Duration::from_secs(8);

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
    timeout(WAIT, async {
        loop {
            assert!(
                daemon.try_wait().unwrap().is_none(),
                "daemon exited during startup"
            );
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
    .unwrap();
    daemon
}
