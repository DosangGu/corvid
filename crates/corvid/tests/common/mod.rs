use std::path::Path;
use std::time::Duration;

use corvid_protocol::unix::{RuntimeDir, SocketRole};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

const BINARY: &str = env!("CARGO_BIN_EXE_corvid");
pub const WAIT: Duration = Duration::from_secs(8);

pub fn command(runtime: &Path) -> Command {
    let mut command = Command::new(BINARY);
    command.arg("--runtime-dir").arg(runtime).kill_on_drop(true);
    command
}

pub async fn daemon(runtime: &Path, state: &Path) -> Child {
    let mut daemon = command(runtime)
        .arg("daemon")
        .arg("--state-dir")
        .arg(state)
        .spawn()
        .unwrap();
    timeout(WAIT, async {
        loop {
            assert!(
                daemon.try_wait().unwrap().is_none(),
                "daemon exited during startup"
            );
            if let Ok(directory) = RuntimeDir::open(runtime)
                && directory.connect(SocketRole::Control).await.is_ok()
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
