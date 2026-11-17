use std::process::Stdio;

use corvid_agent_deck::desktop::FocusRequest;
use corvid_companion::HostEndpoint;
use tokio::process::{Child, Command};
use tokio::task::JoinSet;

use super::*;

struct FixtureHost {
    run: PresentedRunKey,
    requests: mpsc::Receiver<FocusRequest>,
}

impl FixtureHost {
    fn start(
        root: &Path,
        id: Uuid,
        endpoint: HostEndpoint,
        run_id: Uuid,
        jobs: &mut JoinSet<Result<()>>,
    ) -> Result<Self> {
        let state = match endpoint {
            HostEndpoint::Local { .. } => root.to_owned(),
            _ => root.join("slot-board").join(id.simple().to_string()),
        };
        corvid_companion::initialize(&state, id, endpoint, 8)?;
        let config = Config::read_if_present(&state)?.context("fixture config")?;
        let controller = config.controller_id;
        let address = root.join(&controller.simple().to_string()[..6]);
        let scope = corvid_companion::monitor::Scope {
            state,
            config,
            address: address.clone(),
        };
        scope.configure(&corvid_client::ControlEndpoint::Local(root.join("unused")))?;
        let epoch = Uuid::new_v4();
        let run = PresentedRunKey {
            host_id: id.to_string(),
            remote_epoch: epoch.to_string(),
            agent_run_id: run_id.to_string(),
        };
        let (_, feed) = watch::channel(Vec::new());
        jobs.spawn(serve(address, controller, run.clone(), feed));
        let (focus, requests) = mpsc::channel(1);
        jobs.spawn(corvid_presentation::desktop_focus::serve(
            id, controller, epoch, focus,
        ));
        Ok(Self { run, requests })
    }
}

#[tokio::test]
#[ignore = "requires an isolated X11 display and a built corvid-agent-deck binary"]
async fn grid_routes_clicks_to_local_and_two_ssh_scopes_including_a_late_host() -> Result<()> {
    let root = tempfile::tempdir_in(std::env::home_dir().context("home")?)?;
    let state = root.path().join("companion");
    let mut jobs = JoinSet::new();
    // Sharing a provider run ID deliberately rules out lookup by a label or an
    // unqualified run. Metadata and click delivery use real local IPC sockets.
    let common_run = Uuid::new_v4();
    let mut local = FixtureHost::start(
        &state,
        Uuid::from_u128(1),
        HostEndpoint::from(&corvid_client::ControlEndpoint::Local(
            root.path().join("daemon"),
        )),
        common_run,
        &mut jobs,
    )?;
    let mut first = FixtureHost::start(
        &state,
        Uuid::from_u128(2),
        HostEndpoint::Ssh {
            alias: "first".into(),
        },
        common_run,
        &mut jobs,
    )?;
    let _manager = Command::new("openbox")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let built = std::env::current_exe()?
        .parent()
        .and_then(Path::parent)
        .context("test build directory")?
        .join("corvid-agent-deck");
    let mut deck = Command::new(built)
        .arg("--state-dir")
        .arg(&state)
        .kill_on_drop(true)
        .spawn()?;
    let window = window_id(&mut deck).await?;
    click_card(&window, 0, &mut local, &mut jobs).await?;
    click_card(&window, 1, &mut first, &mut jobs).await?;

    let mut second = FixtureHost::start(
        &state,
        Uuid::from_u128(3),
        HostEndpoint::Ssh {
            alias: "late".into(),
        },
        common_run,
        &mut jobs,
    )?;
    click_card(&window, 2, &mut second, &mut jobs).await?;
    if let Some(path) = std::env::var_os("CORVID_MULTI_HOST_GRID_SCREENSHOT") {
        let status = Command::new("scrot")
            .args(["--overwrite", "--focused"])
            .arg(path)
            .status()
            .await?;
        ensure!(status.success(), "capturing the multi-host grid");
    }
    deck.kill().await?;
    println!("PASS: Local and two SSH cards sent the exact host, epoch, and agent ID");
    Ok(())
}

async fn window_id(deck: &mut Child) -> Result<String> {
    timeout(Duration::from_secs(10), async {
        loop {
            ensure!(deck.try_wait()?.is_none(), "Deck stopped during startup");
            let shown = Command::new("xdotool")
                .args(["search", "--onlyvisible", "--name", "^Corvid Agent Deck$"])
                .output()
                .await?;
            if shown.status.success()
                && let Some(id) = String::from_utf8(shown.stdout)?.lines().next()
            {
                let status = Command::new("xdotool")
                    .args(["windowactivate", "--sync", id])
                    .status()
                    .await?;
                if status.success() {
                    return Ok(id.to_owned());
                }
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("opening the multi-host grid")?
}

async fn click_card(
    window: &str,
    index: u32,
    expected: &mut FixtureHost,
    jobs: &mut JoinSet<Result<()>>,
) -> Result<()> {
    let x = (55 + index * 94).to_string();
    timeout(Duration::from_secs(12), async {
        loop {
            let status = Command::new("xdotool")
                .args(["mousemove", "--window", window, &x, "85", "click", "1"])
                .status().await?;
            ensure!(status.success(), "clicking the Deck card");
            tokio::select! {
                request = expected.requests.recv() => {
                    let request = request.context("Desktop focus request stream closed")?;
                    assert_eq!(request.run, expected.run);
                    request.result.send(Ok(())).map_err(|_| anyhow::anyhow!("Deck stopped waiting"))?;
                    return Ok(());
                }
                result = jobs.join_next() => anyhow::bail!("fixture server stopped: {result:?}"),
                _ = sleep(Duration::from_millis(350)) => {}
            }
        }
    }).await.with_context(|| format!("routing card {index} to {}", expected.run.host_id))?
}
