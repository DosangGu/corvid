use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::ptr::{null, null_mut};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_agent_deck::{connection, desktop::Settings, lifecycle};
use corvid_async::time::{sleep, timeout};
use corvid_async::{mpsc, oneshot};
use corvid_companion::{Config, HostEndpoint};
use corvid_protocol::windows::StateDirectory;
use corvid_protocol::wire::{AgentDeckOpened, envelope::Body};
use corvid_protocol::{COMMAND_TIMEOUT, PeerRole, accept_hello};
use uuid::Uuid;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.iter().any(|arg| arg == "--version") {
        let directory = std::env::current_exe()?.parent().unwrap().to_owned();
        println!(
            "corvid-agent-deck {}",
            std::fs::read_to_string(directory.join("version.txt"))?
        );
        return Ok(());
    }
    if let Some(index) = args.iter().position(|arg| arg == "--state-dir") {
        return fixture(
            Path::new(&args[index + 1]),
            args.iter().any(|arg| arg == "--background"),
        );
    }
    compio::runtime::Runtime::new()?.block_on(checks())
}

async fn checks() -> Result<()> {
    println!("Checking running Deck replacement on Windows");
    let directory = tempfile::tempdir()?;
    let old = executable(directory.path(), "old", "0.1.4")?;
    let current = executable(directory.path(), "current", env!("CARGO_PKG_VERSION"))?;
    let same_version = executable(directory.path(), "same-version", env!("CARGO_PKG_VERSION"))?;
    let newer = executable(directory.path(), "newer", "999.0.0")?;
    let settings = Settings {
        state: directory.path().join("state"),
        pipe_instance: Uuid::new_v4().simple().to_string(),
    };
    let config = initialize(&settings.state)?;
    let another_state = directory.path().join("unrelated");
    let another_config = initialize(&another_state)?;
    let mut another = spawn(&old, &another_state, &another_config, true)
        .await
        .context("starting unrelated controller")?;
    let mut cleanup = Cleanup(vec![
        (settings.state.clone(), config.controller_id),
        (another_state.clone(), another_config.controller_id),
    ]);

    // Startup must not revive a Deck the user has quit.
    settings
        .refresh_running(&current)
        .await
        .context("checking stopped Deck")?;
    assert!(!settings.state.join("started.json").exists());

    let storage = StateDirectory::create_all(&settings.state.join("agent-deck"))?;
    storage.replace_private_file(
        "window.json",
        br#"{"pinned":false,"layout":{"preset":"eight","orientation":"portrait"}}"#,
    )?;
    storage.replace_private_file("notifications.json", b"preserved notification fixture")?;
    let saved_config = std::fs::read(settings.state.join("config.json"))?;
    let saved_window = std::fs::read(settings.state.join("agent-deck/window.json"))?;
    let mut previous = spawn(&old, &settings.state, &config, true)
        .await
        .context("starting old Deck")?;
    let old_pid = previous.id();
    settings
        .refresh_running(&current)
        .await
        .context("replacing old Deck")?;
    assert!(previous.try_wait()?.is_some());
    assert!(
        settings.state.join(format!("quit-{old_pid}")).is_file(),
        "must use orderly WM_QUIT cleanup"
    );
    let replacement = started(&settings.state)?;
    assert_eq!(
        replacement["executable"],
        current.to_string_lossy().as_ref()
    );
    assert_eq!(replacement["background"], true);
    assert_eq!(
        std::fs::read(settings.state.join("config.json"))?,
        saved_config
    );
    assert_eq!(
        std::fs::read(settings.state.join("agent-deck/window.json"))?,
        saved_window
    );
    assert_eq!(
        std::fs::read(settings.state.join("agent-deck/notifications.json"))?,
        b"preserved notification fixture"
    );
    assert!(
        another.try_wait()?.is_none(),
        "another controller must remain running"
    );

    // Concurrent sweeps and an Agents click must reuse the one replacement.
    let (first, second) = corvid_async::join!(
        settings.refresh_running(&current),
        lifecycle::ensure_current(
            &current,
            &settings.state,
            &settings.pipe_instance,
            &config,
            true
        )
    );
    first?;
    second?.unwrap().activate(true).await?;
    assert_eq!(started(&settings.state)?["pid"], replacement["pid"]);
    settings.refresh_running(&same_version).await?;
    assert_eq!(started(&settings.state)?["pid"], replacement["pid"]);
    let mut wrong_host = Config::read_if_present(&settings.state)?.unwrap();
    wrong_host.host_id = Uuid::new_v4();
    assert!(
        lifecycle::ensure_current(
            &current,
            &settings.state,
            &settings.pipe_instance,
            &wrong_host,
            true
        )
        .await
        .is_err()
    );
    assert_eq!(started(&settings.state)?["pid"], replacement["pid"]);
    stop(&settings.state).await?;

    // An older Desktop must never downgrade a newer running Deck.
    let mut future = spawn(&newer, &settings.state, &config, true).await?;
    settings.refresh_running(&current).await?;
    assert!(future.try_wait()?.is_none());
    assert_eq!(started(&settings.state)?["pid"], future.id());
    stop(&settings.state).await?;

    // Host-specific saved controllers are discovered even without an Agents click.
    let scoped_host = Uuid::new_v4();
    let scoped_state = settings
        .state
        .join("slot-board")
        .join(scoped_host.simple().to_string());
    corvid_companion::initialize(
        &scoped_state,
        scoped_host,
        HostEndpoint::Ssh {
            alias: "deck-update-fixture.invalid".into(),
        },
        8,
    )?;
    let scoped = Config::read_if_present(&scoped_state)?.unwrap();
    cleanup.0.push((scoped_state.clone(), scoped.controller_id));
    let mut scoped_old = spawn(&old, &scoped_state, &scoped, false).await?;
    settings.refresh_running(&current).await?;
    assert!(scoped_old.try_wait()?.is_some());
    assert_eq!(started(&scoped_state)?["background"], false);
    assert_eq!(
        started(&scoped_state)?["executable"],
        current.to_string_lossy().as_ref()
    );
    stop(&scoped_state).await?;
    stop(&another_state).await?;
    cleanup.0.clear();
    println!(
        "Windows Deck update: old process replaced; hidden/visible state, preferences, concurrency, scope and newer-version reuse verified"
    );
    Ok(())
}

fn executable(root: &Path, name: &str, version: &str) -> Result<PathBuf> {
    let directory = root.join(name);
    std::fs::create_dir(&directory)?;
    let path = directory.join("corvid-agent-deck.exe");
    std::fs::copy(std::env::current_exe()?, &path)?;
    std::fs::write(directory.join("version.txt"), version)?;
    Ok(path)
}

fn initialize(state: &Path) -> Result<Config> {
    corvid_companion::initialize(
        state,
        Uuid::new_v4(),
        HostEndpoint::Ssh {
            alias: "deck-update-fixture.invalid".into(),
        },
        8,
    )?;
    Ok(Config::read_if_present(state)?.unwrap())
}

async fn spawn(exe: &Path, state: &Path, config: &Config, background: bool) -> Result<Child> {
    let mut command = Command::new(exe);
    command
        .arg("--state-dir")
        .arg(state)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if background {
        command.arg("--background");
    }
    let mut child = command.spawn()?;
    let ready = timeout(COMMAND_TIMEOUT, async {
        loop {
            if state.join("started.json").is_file()
                && started(state)?["pid"] == child.id()
                && connection::Client::connect(config.host_id, config.controller_id, false)
                    .await
                    .is_ok()
            {
                return Ok::<_, anyhow::Error>(());
            }
            ensure!(child.try_wait()?.is_none(), "fixture exited before ready");
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .context("fixture readiness timed out")
    .and_then(|result| result);
    if let Err(error) = ready {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(child)
}

fn started(state: &Path) -> Result<serde_json::Value> {
    Ok(serde_json::from_slice(&std::fs::read(
        state.join("started.json"),
    )?)?)
}

async fn stop(state: &Path) -> Result<()> {
    let info = started(state)?;
    let thread = info["thread"].as_u64().context("fixture thread")? as u32;
    ensure!(
        unsafe { PostThreadMessageW(thread, WM_QUIT, 0, 0) } != 0,
        "cannot stop fixture"
    );
    let marker = state.join(format!("quit-{}", info["pid"]));
    timeout(COMMAND_TIMEOUT, async {
        while !marker.exists() {
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await?;
    // Wait until the private pipe has also gone, before another fixture starts.
    let config = Config::read_if_present(state)?.unwrap();
    timeout(COMMAND_TIMEOUT, async {
        while corvid_protocol::windows::open_scoped(
            "corvid-agent-deck-v1",
            &config.controller_id.simple().to_string(),
        )
        .await
        .is_ok()
        {
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await?;
    Ok(())
}

struct Cleanup(Vec<(PathBuf, Uuid)>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for (state, _) in &self.0 {
            if let Ok(info) = started(state)
                && let Some(thread) = info["thread"].as_u64()
            {
                unsafe {
                    PostThreadMessageW(thread as u32, WM_QUIT, 0, 0);
                }
            }
        }
    }
}

fn fixture(state: &Path, background: bool) -> Result<()> {
    let config = Config::read_if_present(state)?.context("fixture config")?;
    let module = unsafe { GetModuleHandleW(null()) };
    let class: Vec<u16> = "Zed::Window\0".encode_utf16().collect();
    let wc = WNDCLASSW {
        lpfnWndProc: Some(DefWindowProcW),
        hInstance: module,
        lpszClassName: class.as_ptr(),
        ..Default::default()
    };
    ensure!(unsafe { RegisterClassW(&wc) } != 0, "fixture class");
    let window = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            64,
            64,
            null_mut(),
            null_mut(),
            module,
            null(),
        )
    };
    ensure!(!window.is_null(), "fixture window");
    if !background {
        unsafe {
            ShowWindow(window, SW_SHOWNOACTIVATE);
        }
    }
    let (ready, started_rx) = std::sync::mpsc::sync_channel(1);
    let state_directory = state.to_owned();
    std::thread::spawn(move || {
        let runtime = compio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let (peers, mut incoming) = mpsc::channel(8);
            let (listening, listening_rx) = oneshot::channel();
            corvid_async::task::spawn(connection::listen(config.controller_id, peers, listening));
            listening_rx.await.unwrap();
            ready.send(()).unwrap();
            while let Some(peer) = incoming.recv().await {
                let host = config.host_id;
                corvid_async::task::spawn(async move {
                    let result = async {
                        let (mut reader, mut writer) = connection::frames(peer.stream).await?;
                        let hello = reader.next_envelope().await?.context("fixture Hello")?;
                        let (session, reply) = accept_hello(
                            &hello,
                            PeerRole::Presentation,
                            &host.to_string(),
                            &Uuid::new_v4().to_string(),
                            &[connection::CAPABILITY],
                        )?;
                        writer.envelope(&reply).await?;
                        while let Some(request) = reader.next_envelope().await? {
                            session.validate_scope(&request)?;
                            ensure!(
                                matches!(request.body, Some(Body::OpenAgentDeck(_))),
                                "fixture request"
                            );
                            writer
                                .envelope(&session.envelope(
                                    request.request_id,
                                    Body::AgentDeckOpened(AgentDeckOpened {}),
                                ))
                                .await?;
                        }
                        Ok::<_, anyhow::Error>(())
                    }
                    .await;
                    let _ = result;
                });
            }
        });
    });
    started_rx.recv_timeout(COMMAND_TIMEOUT)?;
    let info = serde_json::json!({ "pid": std::process::id(), "thread": unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }, "executable": std::env::current_exe()?, "background": background });
    std::fs::write(state.join("started.json"), serde_json::to_vec(&info)?)?;
    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } > 0 {
        unsafe {
            DispatchMessageW(&message);
        }
    }
    std::fs::write(
        state_directory.join(format!("quit-{}", std::process::id())),
        b"orderly exit",
    )?;
    unsafe {
        DestroyWindow(window);
    }
    Ok(())
}
