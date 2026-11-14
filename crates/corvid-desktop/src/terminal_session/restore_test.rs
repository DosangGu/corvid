use super::*;
use std::path::Path;

const TEST: &str = "terminal_session::restore_test::desktop_relaunch_restores_the_same_terminal";

// Run the backend in a subprocess so environment overrides never affect other tests.
#[test]
fn desktop_relaunch_restores_the_same_terminal() -> Result<()> {
    if let Some(root) = std::env::var_os("CORVID_DESKTOP_RESTORE_FIXTURE") {
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(run_fixture(Path::new(&root)));
    }
    let fixture = tempfile::Builder::new()
        .prefix("corvid-desktop-view-test-")
        .tempdir()?;
    let output = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", TEST, "--nocapture"])
        .env("CORVID_DESKTOP_RESTORE_FIXTURE", fixture.path())
        .env("CORVID_RUNTIME_DIR", fixture.path().join("runtime"))
        .output()?;
    ensure!(
        output.status.success(),
        "Desktop restore fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

async fn run_fixture(root: &Path) -> Result<()> {
    let runtime = root.join("runtime");
    let state = root.join("daemon");
    let (stop, stopped) = oneshot::channel();
    let server_runtime = runtime.clone();
    let server = tokio::spawn(async move {
        let shutdown = async {
            let _ = stopped.await;
        };
        #[cfg(windows)]
        {
            corvid_daemon::run(&server_runtime, &state, shutdown).await
        }
        #[cfg(target_os = "linux")]
        {
            corvid_daemon::run(&server_runtime, &state, false, shutdown).await
        }
    });
    let result = async {
        timeout(Duration::from_secs(15), async {
            while corvid_client::ping(&runtime).await.is_err() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await?;
        verify_restore(&root.join("desktop")).await
    }
    .await;
    let _ = stop.send(());
    timeout(Duration::from_secs(15), server).await???;
    result
}

struct AttachedSession {
    session: Session,
    job: Option<Job>,
}

impl AttachedSession {
    fn start(state: &Path) -> Result<Self> {
        let (session, job, _prompts) = Session::start(
            Destination::Local,
            state.to_owned(),
            AttachTarget::Default,
            false,
        )?;
        Ok(Self {
            session,
            job: Some(job),
        })
    }
}

impl Drop for AttachedSession {
    fn drop(&mut self) {
        if let Some(mut job) = self.job.take() {
            job.stop();
            job.join();
        }
    }
}

async fn wait_view(session: &Session, ready: impl Fn(&Ui) -> bool) -> Result<Ui> {
    let mut changes = session.changes();
    timeout(Duration::from_secs(20), async {
        loop {
            let ui = session.ui();
            if ui.connected && !ui.changing && ui.inventory_ready && ready(&ui) {
                return Ok(ui);
            }
            ensure!(ui.changing || ui.connected, "{}", ui.message);
            changes.changed().await?;
        }
    })
    .await
    .context("waiting for Desktop view")?
}

async fn wait_screen(session: &Session, pane: &str, marker: &str) -> Result<()> {
    let mut changes = session.screen_changes();
    timeout(Duration::from_secs(20), async {
        loop {
            if session.screen(pane, |screen| screen.contents().contains(marker)) == Some(true) {
                return Ok(());
            }
            changes.changed().await?;
        }
    })
    .await
    .context("waiting for Desktop terminal output")?
}

async fn wait_disconnected(session: &Session) -> Result<Ui> {
    let mut changes = session.changes();
    timeout(Duration::from_secs(20), async {
        loop {
            let ui = session.ui();
            if !ui.connected && !ui.changing {
                return Ok(ui);
            }
            changes.changed().await?;
        }
    })
    .await
    .context("waiting for Desktop detach")?
}

async fn verify_restore(state: &Path) -> Result<()> {
    let first = AttachedSession::start(state)?;
    let initial = wait_view(&first.session, |_| true).await?.selected.unwrap();
    first
        .session
        .create(Target::WorkspaceLabel("Restore workspace".into()));
    let workspace = wait_view(&first.session, |ui| {
        ui.selected
            .as_ref()
            .is_some_and(|view| view.workspace_id != initial.workspace_id)
    })
    .await?
    .selected
    .unwrap();
    first.session.create(Target::TabLabel("Restore tab".into()));
    let tab = wait_view(&first.session, |ui| {
        ui.selected
            .as_ref()
            .is_some_and(|view| view.tab_id != workspace.tab_id)
    })
    .await?
    .selected
    .unwrap();
    first.session.create(Target::SplitAxis(
        corvid_protocol::wire::SplitAxis::Horizontal as i32,
    ));
    let selected = wait_view(&first.session, |ui| {
        ui.selected
            .as_ref()
            .is_some_and(|view| view.pane_id != tab.pane_id)
    })
    .await?;
    let view = selected.selected.as_ref().unwrap();
    wait_screen(&first.session, &view.pane_id, "").await?;
    #[cfg(windows)]
    let command =
        b"$restoreValue='kept'; [Console]::WriteLine(('DESKTOP_' + $restoreValue))\r".to_vec();
    #[cfg(target_os = "linux")]
    let command = b"restore_value=kept; printf 'DESKTOP_%s\\n' \"$restore_value\"\n".to_vec();
    ensure!(
        first.session.send(&view.pane_id, command, false),
        "input was not accepted"
    );
    wait_screen(&first.session, &view.pane_id, "DESKTOP_kept").await?;
    drop(first);

    let store = ViewStore::new(state.join("desktop-view"));
    let saved = store.last().context("Desktop did not save its view")?;
    ensure!(saved.location.pane.to_string() == view.pane_id);
    ensure!(matches!(
        Destination::from_saved(saved.target)?,
        Destination::Local
    ));
    let second = AttachedSession::start(state)?;
    let restored = wait_view(&second.session, |_| true).await?;
    ensure!(
        restored.selected == selected.selected && restored.terminal_id == selected.terminal_id,
        "Desktop changed the restored selection or terminal"
    );
    wait_screen(&second.session, &view.pane_id, "DESKTOP_kept").await?;
    #[cfg(windows)]
    let command = b"[Console]::WriteLine(('CHECK_' + $restoreValue))\r".to_vec();
    #[cfg(target_os = "linux")]
    let command = b"printf 'CHECK_%s\\n' \"$restore_value\"\n".to_vec();
    ensure!(second.session.send(&view.pane_id, command, false));
    wait_screen(&second.session, &view.pane_id, "CHECK_kept").await?;
    ensure!(
        second
            .session
            .send(&view.pane_id, b"exit\r".to_vec(), false)
    );
    wait_disconnected(&second.session).await?;
    drop(second);

    let empty = AttachedSession::start(state)?;
    let ui = wait_disconnected(&empty.session).await?;
    ensure!(
        ui.restore_unavailable,
        "restoration unexpectedly opened a new terminal: {}",
        ui.message
    );
    ensure!(
        ui.inventory
            .as_ref()
            .is_some_and(|inventory| !inventory.live(view)),
        "the ended terminal was restarted"
    );
    ensure!(
        store
            .last()
            .is_some_and(|saved| saved.location.pane.to_string() == view.pane_id),
        "failed restoration overwrote the saved view"
    );
    drop(empty);
    Ok(())
}
