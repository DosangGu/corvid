use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

pub(super) struct Running {
    #[cfg(windows)]
    pub pid: u32,
    pub image: PathBuf,
    pub args: Vec<OsString>,
}

impl Running {
    pub fn option(&self, name: &str) -> Option<&OsStr> {
        self.args
            .windows(2)
            .find(|pair| pair[0] == name)
            .map(|pair| pair[1].as_os_str())
    }
    pub fn daemon(&self) -> bool {
        self.args.first().is_some_and(|a| a == "daemon")
            && self.args.iter().any(|a| a == "--autostarted")
    }
    pub fn monitor(&self) -> bool {
        self.args.first().is_some_and(|a| a == "companion")
            && self.args.last().is_some_and(|a| a == "run")
    }
}

pub(super) fn snapshot(packages: &[PathBuf], runtime: &Path) -> Result<Vec<Running>> {
    let mut system = System::new();
    let images = ProcessRefreshKind::new().with_exe(UpdateKind::Always);
    system.refresh_processes_specifics(ProcessesToUpdate::All, images);
    ensure!(
        system
            .process(sysinfo::Pid::from_u32(std::process::id()))
            .and_then(|p| p.exe())
            .is_some(),
        "could not enumerate local executable images"
    );
    let mut selected = Vec::new();
    for (pid, p) in system.processes() {
        if let Some(image) = p.exe()
            && let Ok(image) = dunce::canonicalize(image)
            && (packages.iter().any(|dir| image.starts_with(dir))
                || image.parent() == Some(runtime)
                || (cfg!(windows)
                    && image
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("pty-host-"))))
        {
            selected.push((*pid, image));
        }
    }
    let pids: Vec<_> = selected.iter().map(|(pid, _)| *pid).collect();
    if !pids.is_empty() {
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            images.with_cmd(UpdateKind::Always),
        );
    }
    let mut entries = Vec::new();
    for (pid, image) in selected {
        if let Some(p) = system.process(pid)
            && p.exe().and_then(|p| dunce::canonicalize(p).ok()).as_ref() == Some(&image)
        {
            entries.push(Running {
                #[cfg(windows)]
                pid: pid.as_u32(),
                image,
                args: p.cmd().iter().skip(1).cloned().collect(),
            });
        }
    }
    Ok(entries)
}

pub(super) async fn handoff(process: &Running, runtime: &Path) -> Result<()> {
    use corvid_client::ControlEndpoint;
    let runtime = process
        .option("--runtime-dir")
        .map(Path::new)
        .unwrap_or(runtime);
    // Correlate the package image with the actual server on this endpoint.
    #[cfg(windows)]
    let image = crate::windows::daemon_image(runtime).await?;
    #[cfg(unix)]
    let image = {
        use corvid_protocol::unix::{RuntimeDir, SocketRole, peer_pid, process_image_path};
        let socket = RuntimeDir::open(runtime)?
            .connect(SocketRole::Control)
            .await?;
        process_image_path(peer_pid(&socket)?)?
    };
    ensure!(
        dunce::canonicalize(image)? == process.image,
        "daemon image changed"
    );
    let endpoint = ControlEndpoint::Local(runtime.to_owned());
    let link = endpoint.interactive(Vec::new()).await?;
    let session = link.session();
    if let Some(version) = session.daemon_version.as_deref()
        && corvid_update::version(version)? > corvid_update::version(env!("CARGO_PKG_VERSION"))?
    {
        return Ok(());
    }
    let host = session.host_id.parse()?;
    let epoch = session.server_epoch.parse()?;
    drop(link);
    #[cfg(windows)]
    crate::windows::desktop_handoff(runtime, host, epoch).await?;
    #[cfg(unix)]
    crate::linux::desktop_handoff(runtime, host, epoch).await?;
    let link = endpoint.interactive(Vec::new()).await?;
    let replacement = link.session();
    ensure!(
        replacement.host_id == host.to_string()
            && replacement.server_epoch != epoch.to_string()
            && replacement.daemon_version.as_deref() == Some(env!("CARGO_PKG_VERSION")),
        "daemon replacement did not confirm the new version"
    );
    Ok(())
}

pub(super) async fn monitor(process: &Running, executable: &Path) -> Result<()> {
    let Some(state) = process.option("--state-dir").map(Path::new) else {
        return Ok(());
    };
    let scope = corvid_companion::monitor::discover(state)?
        .into_iter()
        .find(|s| s.state == state);
    if let Some(scope) = scope {
        scope.refresh_running(executable).await?;
    }
    Ok(())
}

pub(super) async fn deck(process: &Running, executable: &Path) -> Result<()> {
    if process
        .args
        .iter()
        .any(|a| a == "--companion-service" || a == "--refresh-running")
    {
        return Ok(());
    }
    let binary = corvid_update::layout::agent_deck_executable(executable)?;
    let mut command = tokio::process::Command::new(binary);
    command.arg("--refresh-running");
    for option in [
        "--state-dir",
        "--pipe-instance",
        "--socket-dir",
        "--socket-name",
    ] {
        if let Some(value) = process.option(option) {
            command.arg(option).arg(value);
        }
    }
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let status = command.status().await?;
    ensure!(
        status.success(),
        "running Agent Deck could not be refreshed"
    );
    Ok(())
}

#[cfg(windows)]
pub(super) async fn legacy_desktop(
    process: &Running,
    package: &Path,
    executable: &Path,
) -> Result<()> {
    if process.image != package.join("corvid-desktop.exe") {
        return Ok(());
    }
    let mut args = process.args.chunks_exact(2);
    if !args.remainder().is_empty()
        || !args.all(|pair| {
            matches!(
                pair[0].to_str(),
                Some("--state-dir" | "--pipe-instance" | "--ssh-alias" | "--wsl-distribution")
            )
        })
    {
        return Ok(());
    }
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(package.join("package.json"))?)?;
    if metadata["update_protocol"].as_u64().is_some() {
        return Ok(());
    }
    corvid_agent_deck::lifecycle::close_legacy_desktop(process.pid, &process.image).await?;
    let installed = corvid_update::Installed {
        executable: executable.to_owned(),
        message: String::new(),
    };
    // The replacement restores saved windows. An explicit SSH destination would
    // select only one of them.
    let mut args = Vec::new();
    for option in ["--state-dir", "--pipe-instance"] {
        if let Some(value) = process.option(option) {
            args.extend([OsString::from(option), value.to_owned()]);
        }
    }
    tokio::task::spawn_blocking(move || installed.start_desktop_with_args(args)).await??;
    Ok(())
}
