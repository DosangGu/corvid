#![cfg(any(windows, target_os = "linux", target_os = "macos"))]

use std::num::NonZeroU16;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use corvid_client::{ControlEndpoint, InteractiveConnection};
use corvid_protocol::wire::create_view::Target;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    Attach, AttachRole, BrowseDirectories, CreateView, FrameApplied, InputChunk, LaunchCommand,
    RouteToken, SplitAxis, ViewSelection, Viewport, WorkspaceCreation,
};
use corvid_protocol::{
    CREATE_VIEW_CAPABILITY, Frame, WORKSPACE_DIRECTORY_CAPABILITY, decode_envelope,
};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

const WAIT: Duration = Duration::from_secs(18);

#[tokio::test]
async fn folder_picker_and_fresh_terminals_use_the_execution_hosts_workspace_directory()
-> Result<()> {
    #[cfg(windows)]
    let fixture = tempfile::tempdir()?;
    #[cfg(unix)]
    let fixture = tempfile::tempdir_in(std::env::home_dir().context("no home folder")?)?;
    let runtime = fixture.path().join("run");
    let state = fixture.path().join("state");
    let workspace = fixture.path().join("project 한글");
    std::fs::create_dir(&workspace)?;
    let other = workspace.join("other");
    std::fs::create_dir(&other)?;
    std::fs::write(fixture.path().join("project file"), "")?;

    let endpoint = ControlEndpoint::Local(runtime.clone());
    let mut daemon = start(&runtime, &state, &endpoint)
        .await
        .context("starting the first daemon")?;
    let mut link = open(&endpoint).await?;
    let prefix = fixture.path().join("project");
    let matches = link
        .browse_directories(BrowseDirectories {
            path: prefix.to_string_lossy().into_owned(),
            complete: true,
            ..Default::default()
        })
        .await?;
    ensure!(matches.entries.len() == 1 && matches.entries[0].name == "project 한글");
    let selected_path = matches.entries[0].path.clone();

    let invalid = attach_request(
        Some("Invalid".into()),
        Some(format!("{selected_path}-gone")),
    );
    let id = link.send(Body::Attach(invalid)).await?;
    let Body::Error(error) = reply(&mut link, &id).await? else {
        bail!("missing folder error")
    };
    ensure!(error.code == "DirectoryNotFound");
    drop(link);
    let mut link = open(&endpoint).await?;
    let initial = attach_request(Some("Project".into()), Some(selected_path.clone()));
    let (_, mut route) = attach(&mut link, initial).await?;
    let mut sequence = 1;
    assert_directory(&mut link, &route, &mut sequence, &workspace).await?;
    send_line(&mut link, &route, &mut sequence, b"cd other").await?;
    assert_directory(&mut link, &route, &mut sequence, &other).await?;

    for target in [
        Target::SplitAxis(SplitAxis::Horizontal as i32),
        Target::SplitAxis(SplitAxis::Vertical as i32),
        Target::TabLabel("Fresh tab".into()),
    ] {
        (_, route) = create(&mut link, &route, target).await?;
        sequence = 1;
        assert_directory(&mut link, &route, &mut sequence, &workspace).await?;
    }
    let (second, route) = create(
        &mut link,
        &route,
        Target::Workspace(WorkspaceCreation {
            label: "Other".into(),
            directory: other.to_string_lossy().into_owned(),
        }),
    )
    .await?;
    sequence = 1;
    assert_directory(&mut link, &route, &mut sequence, &other).await?;
    drop(link);

    let second_id = &second.workspace_id;
    timeout(WAIT, async {
        loop {
            if std::fs::read_to_string(state.join("state.json"))
                .is_ok_and(|metadata| metadata.contains(second_id))
            {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("waiting for the workspace metadata to be saved")?;
    daemon.kill().await?;
    let _daemon = start(&runtime, &state, &endpoint)
        .await
        .context("restarting the daemon")?;
    let mut link = open(&endpoint).await?;
    let (view, route) = attach(
        &mut link,
        Attach {
            workspace_id: Some(second.workspace_id.clone()),
            create_tab: Some("After restart".into()),
            ..attach_request(None, None)
        },
    )
    .await?;
    ensure!(view.workspace_id == second.workspace_id);
    sequence = 1;
    assert_directory(&mut link, &route, &mut sequence, &other).await?;
    drop(link);

    let mut link = open(&endpoint).await?;
    let ask = Attach {
        workspace_id: Some(second.workspace_id),
        create_tab: Some("Override".into()),
        launch: Some(LaunchCommand {
            cwd: Some(workspace.as_os_str().as_encoded_bytes().to_vec()),
            ..Default::default()
        }),
        ..attach_request(None, None)
    };
    let (_, route) = attach(&mut link, ask).await?;
    sequence = 1;
    assert_directory(&mut link, &route, &mut sequence, &workspace).await
}

async fn start(runtime: &Path, state: &Path, endpoint: &ControlEndpoint) -> Result<Child> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_corvid"));
    command
        .arg("--runtime-dir")
        .arg(runtime)
        .arg("daemon")
        .arg("--state-dir")
        .arg(state);
    #[cfg(unix)]
    command.env("SHELL", "/bin/sh");
    let mut child = command.kill_on_drop(true).spawn()?;
    timeout(WAIT, async {
        loop {
            ensure!(child.try_wait()?.is_none(), "fixture daemon exited");
            if endpoint.host().await.is_ok() {
                return Ok::<_, anyhow::Error>(());
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("waiting for fixture daemon")??;
    Ok(child)
}

async fn open(endpoint: &ControlEndpoint) -> Result<InteractiveConnection> {
    endpoint
        .interactive(vec![
            CREATE_VIEW_CAPABILITY.into(),
            WORKSPACE_DIRECTORY_CAPABILITY.into(),
        ])
        .await
}

fn attach_request(label: Option<String>, directory: Option<String>) -> Attach {
    Attach {
        create_workspace: label,
        workspace_directory: directory,
        viewport: Some(Viewport {
            cols: 120,
            rows: 28,
        }),
        requested_role: AttachRole::Writer as i32,
        terminal_capabilities: [
            "legacy_vt.v1",
            CREATE_VIEW_CAPABILITY,
            corvid_protocol::PANE_WRITERS_CAPABILITY,
        ]
        .map(str::to_owned)
        .into(),
        launch: Some(LaunchCommand::default()),
        ..Default::default()
    }
}

async fn attach(
    link: &mut InteractiveConnection,
    attach: Attach,
) -> Result<(ViewSelection, RouteToken)> {
    let id = link.send(Body::Attach(attach)).await?;
    let Body::Attached(result) = reply(link, &id).await? else {
        bail!("attach rejected")
    };
    let route = result.route_token.context("missing route")?;
    Ok((result.selection.context("missing view")?, route))
}

async fn create(
    link: &mut InteractiveConnection,
    route: &RouteToken,
    target: Target,
) -> Result<(ViewSelection, RouteToken)> {
    let id = link
        .send(Body::CreateView(CreateView {
            target: Some(target),
            expected_view_revision: Some(route.view_revision),
        }))
        .await?;
    let Body::ViewChanged(result) = reply(link, &id).await? else {
        bail!("view creation rejected")
    };
    let route = result.route_token.context("missing route")?;
    Ok((result.selection.context("missing view")?, route))
}

async fn reply(link: &mut InteractiveConnection, id: &str) -> Result<Body> {
    timeout(WAIT, async {
        loop {
            match link.receive().await? {
                Frame::Structured(bytes) => {
                    let envelope = decode_envelope(&bytes)?;
                    if envelope.request_id.as_deref() == Some(id) {
                        return envelope.body.context("missing reply");
                    }
                }
                Frame::Render(frame) => acknowledge(link, frame).await?,
            }
        }
    })
    .await?
}

async fn acknowledge(
    link: &mut InteractiveConnection,
    frame: corvid_protocol::RenderFrame,
) -> Result<()> {
    link.send(Body::FrameApplied(FrameApplied {
        render_generation: frame.header.render_generation,
        frame_id: frame.header.frame_id,
    }))
    .await?;
    Ok(())
}

async fn send_input(
    link: &mut InteractiveConnection,
    route: &RouteToken,
    sequence: u64,
    input: &[u8],
) -> Result<()> {
    link.send(Body::InputChunk(InputChunk {
        route_token: Some(route.clone()),
        input_sequence: sequence,
        data: input.to_vec(),
    }))
    .await?;
    Ok(())
}

async fn send_line(
    link: &mut InteractiveConnection,
    route: &RouteToken,
    sequence: &mut u64,
    line: &[u8],
) -> Result<()> {
    send_input(link, route, *sequence, line).await?;
    // Model the Enter key as its own input event. PowerShell on Windows ARM
    // can leave text unsubmitted when the newline is part of a bulk write.
    #[cfg(windows)]
    sleep(Duration::from_millis(75)).await;
    send_input(link, route, *sequence + 1, b"\r").await?;
    *sequence += 2;
    Ok(())
}

async fn assert_directory(
    link: &mut InteractiveConnection,
    route: &RouteToken,
    sequence: &mut u64,
    expected: &Path,
) -> Result<()> {
    let marker = format!("CWD{}=", *sequence);
    #[cfg(windows)]
    let input = format!("('CW' + 'D{}=') + (Get-Location).ProviderPath", *sequence);
    #[cfg(unix)]
    let input = format!("printf 'CW%s%s\\n' 'D{}=' \"$PWD\"", *sequence);
    let expected = expected.canonicalize()?;
    let mut screen = vt100::Parser::new(
        NonZeroU16::new(28).unwrap(),
        NonZeroU16::new(120).unwrap(),
        0,
    );
    timeout(WAIT, async {
        loop {
            // ConPTY can accept input before PowerShell begins reading it.
            // Use repeatable probes instead of depending on a shell profile.
            send_line(link, route, sequence, input.as_bytes()).await?;
            if let Ok(result) = timeout(
                Duration::from_secs(2),
                read_directory(link, route, &mut screen, &marker, &expected),
            )
            .await
            {
                return result;
            }
        }
    })
    .await
    .with_context(|| {
        format!(
            "expected {expected:?} in terminal. Screen: {:?}",
            screen.screen().contents()
        )
    })?
}

async fn read_directory(
    link: &mut InteractiveConnection,
    route: &RouteToken,
    screen: &mut vt100::Parser,
    marker: &str,
    expected: &Path,
) -> Result<()> {
    loop {
        match link.receive().await? {
            Frame::Render(frame) => {
                if frame.header.view_revision == route.view_revision {
                    screen.process(&frame.ansi);
                }
                acknowledge(link, frame).await?;
                if let Some(path) = screen
                    .screen()
                    .contents()
                    .lines()
                    .find_map(|line| line.strip_prefix(marker))
                {
                    let actual = Path::new(path)
                        .canonicalize()
                        .with_context(|| format!("shell reported {path:?}"))?;
                    ensure!(
                        actual == expected,
                        "expected {expected:?}, shell opened in {actual:?}"
                    );
                    return Ok(());
                }
            }
            Frame::Structured(bytes) => {
                if let Some(Body::Error(error)) = decode_envelope(&bytes)?.body {
                    bail!("{}: {}", error.code, error.message);
                }
            }
        }
    }
}
