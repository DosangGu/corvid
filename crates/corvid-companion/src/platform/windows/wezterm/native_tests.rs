use std::collections::BTreeSet;
use std::{env, fs};

use tokio::process::Child;
use tokio::time::sleep;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;

use super::*;

const READY_FILE: &str = "CORVID_WEZTERM_FIXTURE_READY";

struct FixtureGui {
    process: Child,
    gui: Gui,
}

impl FixtureGui {
    async fn start(executable: &Path, ready: &Path) -> Result<Self> {
        let mut process = Command::new(executable)
            .args([
                "--skip-config",
                "start",
                "--always-new-process",
                "--no-auto-connect",
                "--",
            ])
            .args(pane_command()?)
            .env_remove("WEZTERM_UNIX_SOCKET")
            .env_remove("WEZTERM_PANE")
            .env_remove("WEZTERM_CONFIG_FILE")
            .env(READY_FILE, ready)
            .kill_on_drop(true)
            .spawn()
            .context("launching the fixture WezTerm GUI")?;
        let pid = process.id().context("missing fixture GUI PID")?;
        let socket = timeout(Duration::from_secs(15), async {
            loop {
                ensure!(process.try_wait()?.is_none(), "fixture WezTerm GUI exited");
                match fs::read_to_string(ready) {
                    Ok(socket) => return Ok::<_, anyhow::Error>(PathBuf::from(socket)),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("waiting for the fixture pane's socket")??;

        let gui = Gui::open(&socket)?;
        ensure!(gui.identity.pid == pid, "fixture pane joined another GUI");
        Ok(Self { process, gui })
    }

    async fn initial_pane(&self) -> Result<PaneLocation> {
        let panes = self.gui.panes().await?;
        let [pane] = panes.as_slice() else {
            anyhow::bail!("expected one initial fixture pane");
        };
        Ok(*pane)
    }

    async fn capture(&self, pane_id: u32) -> Result<WezTermRegistration> {
        let socket = self
            .gui
            .identity
            .socket
            .to_str()
            .context("non-UTF-8 fixture socket")?;
        WezTermPane::capture(WezTermTarget {
            gui_socket: socket.to_owned(),
            pane_id: Some(pane_id),
            selection: None,
        })
        .await
    }

    async fn add_pane(&self, command: &[&str]) -> Result<PaneLocation> {
        let child = pane_command()?;
        let mut args = command.to_vec();
        args.push("--");
        args.extend(child.iter().map(String::as_str));
        let output = self.gui.invoke(&args).await?;
        let id = std::str::from_utf8(&output)?.trim().parse()?;
        find_pane(&self.gui.panes().await?, id)
    }

    async fn select(&self, pane: PaneLocation) -> Result<()> {
        self.gui
            .invoke(&["activate-pane", "--pane-id", &pane.pane_id.to_string()])
            .await?;
        self.assert_selected(pane).await
    }

    async fn assert_selected(&self, expected: PaneLocation) -> Result<()> {
        #[derive(Deserialize)]
        struct ListedPane {
            #[serde(flatten)]
            location: PaneLocation,
            is_active: bool,
        }

        let bytes = self.gui.invoke(&["list", "--format", "json"]).await?;
        let panes: Vec<ListedPane> = serde_json::from_slice(&bytes)?;
        let selected: Vec<_> = panes
            .iter()
            .filter(|pane| pane.is_active && pane.location.tab_id == expected.tab_id)
            .map(|pane| pane.location)
            .collect();
        assert_eq!(selected, [expected]);
        Ok(())
    }
}

fn pane_command() -> Result<Vec<String>> {
    let binary = env::current_exe()?
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("non-UTF-8 fixture executable path"))?;
    let (_, module) = module_path!().split_once("::").expect("test module path");
    Ok(vec![
        binary,
        "--ignored".to_owned(),
        "--exact".to_owned(),
        format!("{module}::fixture_pane_child"),
    ])
}

#[test]
#[ignore = "only run as a pane in the native WezTerm fixture"]
fn fixture_pane_child() -> Result<()> {
    let socket =
        PathBuf::from(env::var_os("WEZTERM_UNIX_SOCKET").context("missing WezTerm socket")?);
    let pid = socket_pid(&socket)?;
    // The pane process only waits for its own GUI and has a finite lifetime.
    let gui = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    ensure!(!gui.is_null(), "fixture GUI process is unavailable");
    let gui = unsafe { OwnedHandle::from_raw_handle(gui) };

    if let Some(ready) = env::var_os(READY_FILE) {
        let ready = PathBuf::from(ready);
        if !ready.exists() {
            let staging = ready.with_extension("writing");
            fs::write(
                &staging,
                socket.to_str().context("non-UTF-8 fixture socket")?,
            )?;
            fs::rename(staging, ready)?;
        }
    }
    let wait = unsafe { WaitForSingleObject(gui.as_raw_handle(), 90_000) };
    ensure!(
        matches!(wait, WAIT_OBJECT_0 | WAIT_TIMEOUT),
        "fixture GUI wait failed"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires native Windows and an interactive WezTerm desktop"]
async fn native_wezterm_activation_keeps_exact_gui_and_pane() -> Result<()> {
    let executable =
        PathBuf::from(env::var_os("CORVID_WEZTERM_GUI").context("set CORVID_WEZTERM_GUI")?);
    ensure!(
        executable.is_absolute()
            && executable
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("wezterm-gui.exe")),
        "CORVID_WEZTERM_GUI must name an absolute wezterm-gui.exe path"
    );
    let temporary = tempfile::tempdir()?;
    let mut first = FixtureGui::start(&executable, &temporary.path().join("first")).await?;
    let second = FixtureGui::start(&executable, &temporary.path().join("second")).await?;
    let initial = first.initial_pane().await?;
    let other = second.initial_pane().await?;
    assert_ne!(first.gui.identity.pid, second.gui.identity.pid);
    assert_eq!(initial.pane_id, other.pane_id);

    assert!(discovery::socket_paths()?.contains(&first.gui.identity.socket));
    let discovered = discovery::discover_at([
        first.gui.identity.socket.clone(),
        second.gui.identity.socket.clone(),
    ])
    .await?;
    assert_eq!(discovered.len(), 2);
    let target = discovered
        .iter()
        .find(|pane| pane.location().gui_pid == first.gui.identity.pid)
        .context("missing selected GUI in the pane inventory")?
        .target();
    let saved = first.capture(initial.pane_id).await?;
    assert_eq!(WezTermPane::capture(target.clone()).await?, saved);
    let mut wrong_instance = target.clone();
    wrong_instance.gui_socket = second.gui.identity.socket.to_str().unwrap().to_owned();
    assert!(WezTermPane::capture(wrong_instance).await.is_err());
    let first_split = first
        .add_pane(&["split-pane", "--pane-id", &initial.pane_id.to_string()])
        .await?;
    let second_split = second
        .add_pane(&["split-pane", "--pane-id", &other.pane_id.to_string()])
        .await?;
    first.select(first_split).await?;
    second.select(second_split).await?;

    let registered = WezTermPane::restore(&saved).await?;
    let activated = registered.activate().await;
    println!("Single-window activation: {activated:?}");
    assert!(matches!(
        activated,
        LocalActivation::Activated | LocalActivation::PaneSelectedForegroundUnknown
    ));
    first.assert_selected(initial).await?;
    second.assert_selected(second_split).await?;
    assert_eq!(first.gui.panes().await?.len(), 2);
    assert_eq!(second.gui.panes().await?.len(), 2);

    first
        .gui
        .invoke(&[
            "move-pane-to-new-tab",
            "--pane-id",
            &initial.pane_id.to_string(),
        ])
        .await?;
    let moved = find_pane(&first.gui.panes().await?, initial.pane_id)?;
    assert_ne!(moved.tab_id, initial.tab_id);
    assert!(WezTermPane::capture(target).await.is_err());
    assert!(WezTermPane::restore(&saved).await.is_err());
    assert_eq!(registered.activate().await, LocalActivation::Unavailable);
    second.assert_selected(second_split).await?;

    let moved_registration = first.capture(moved.pane_id).await?;
    first
        .add_pane(&[
            "spawn",
            "--new-window",
            "--pane-id",
            &moved.pane_id.to_string(),
        ])
        .await?;
    let windows: BTreeSet<_> = first
        .gui
        .panes()
        .await?
        .iter()
        .map(|pane| pane.window_id)
        .collect();
    assert_eq!(windows.len(), 2);
    let moved_pane = WezTermPane::restore(&moved_registration).await?;
    assert_eq!(
        moved_pane.activate().await,
        LocalActivation::PaneSelectedForegroundUnknown
    );

    first
        .gui
        .invoke(&["kill-pane", "--pane-id", &moved.pane_id.to_string()])
        .await?;
    assert!(find_pane(&first.gui.panes().await?, moved.pane_id).is_err());
    assert_eq!(moved_pane.activate().await, LocalActivation::Unavailable);

    timeout(Duration::from_secs(5), first.process.kill()).await??;
    let replacement = FixtureGui::start(&executable, &temporary.path().join("replacement")).await?;
    assert_eq!(replacement.initial_pane().await?.pane_id, initial.pane_id);
    assert!(replacement.gui.identity != saved.gui);
    assert!(WezTermPane::restore(&moved_registration).await.is_err());
    assert_eq!(moved_pane.activate().await, LocalActivation::Unavailable);
    second.assert_selected(second_split).await?;
    Ok(())
}
