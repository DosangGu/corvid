use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, ensure};
use corvid_update::daemon;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) fn find() -> Result<Option<PathBuf>> {
    if let Some(executable) = std::env::var_os("CORVID_DAEMON") {
        let executable = PathBuf::from(executable);
        ensure!(
            executable.is_absolute() && executable.is_file(),
            "CORVID_DAEMON must name an absolute path to a daemon executable"
        );
        return Ok(Some(executable));
    }
    let state = corvid_client::daemon_control::state_path(None)?;
    daemon::find(&std::env::current_exe()?, &state, VERSION)
}

pub fn daemon_is_installed() -> Result<bool> {
    Ok(find()?.is_some())
}

fn install() -> Result<PathBuf> {
    daemon::install(&corvid_client::daemon_control::state_path(None)?, VERSION)
}

pub async fn install_local_daemon() -> Result<PathBuf> {
    corvid_async::task::spawn_blocking(install).await?
}

pub(super) async fn ensure_cli(runtime: &Path) -> Result<()> {
    prepare_cli(runtime).await?;
    super::ensure_local_daemon(runtime).await
}

pub(super) async fn prepare_cli(runtime: &Path) -> Result<()> {
    match corvid_client::ping(runtime).await {
        Ok(()) => Ok(()),
        Err(error) if corvid_client::daemon_missing(&error) => require_cli_daemon().await,
        Err(error) => Err(error),
    }
}

pub(super) async fn require_cli_daemon() -> Result<()> {
    if find()?.is_none() {
        confirm_install()?;
        install_local_daemon().await?;
    }
    Ok(())
}

fn confirm_install() -> Result<()> {
    ensure!(
        io::stdin().is_terminal(),
        "The Corvid daemon is missing. Install it from an interactive terminal or use the complete release package."
    );
    eprint!("Download and install Corvid daemon v{VERSION} from GitHub Releases? [y/N] ");
    io::stderr().flush()?;
    let mut decision = String::new();
    use std::io::BufRead;
    io::stdin().lock().read_line(&mut decision)?;
    ensure!(
        matches!(decision.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        "Corvid daemon setup canceled"
    );
    eprintln!("Downloading Corvid daemon v{VERSION}…");
    Ok(())
}

pub(super) fn foreground(runtime: &Path, state: &Path, autostarted: bool) -> Result<u32> {
    let mut args = vec![
        OsString::from("daemon"),
        OsString::from("--runtime-dir"),
        runtime.as_os_str().to_owned(),
        OsString::from("--state-dir"),
        state.as_os_str().to_owned(),
    ];
    #[cfg(unix)]
    args.push("--inherit-terminfo".into());
    if autostarted {
        args.push("--autostarted".into());
    }
    forward_cli(args)
}

pub(super) fn forward_cli(args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Result<u32> {
    let executable = match find()? {
        Some(executable) => executable,
        None => {
            confirm_install()?;
            install()?
        }
    };
    let mut command = Command::new(executable);
    command.args(args);
    forward(command)
}

pub(super) fn legacy_helper() -> Option<ExitCode> {
    let mut args = std::env::args_os();
    let invoked_as = args.next()?;
    #[cfg(unix)]
    let provider = Path::new(&invoked_as)
        .file_name()
        .is_some_and(|name| name == "claude" || name == "codex");
    #[cfg(windows)]
    let provider = {
        let _ = invoked_as;
        false
    };
    let mut args = args.peekable();
    if !provider
        && args
            .peek()
            .is_none_or(|argument| argument != "terminal-host")
    {
        return None;
    }

    let result = super::local_daemon_executable().and_then(|path| {
        let mut command = Command::new(path);
        command.args(args);
        #[cfg(unix)]
        if provider {
            use std::os::unix::process::CommandExt;
            command.arg0(invoked_as);
        }
        forward(command)
    });
    Some(match result {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("corvid: {error:#}");
            ExitCode::FAILURE
        }
    })
}

#[cfg(unix)]
fn forward(mut command: Command) -> Result<u32> {
    use std::os::unix::process::CommandExt;

    Err(command.exec()).context("starting the standalone Corvid daemon")
}

#[cfg(windows)]
fn forward(mut command: Command) -> Result<u32> {
    use std::mem::size_of_val;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::ptr::null;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };

    let handle = unsafe { CreateJobObjectW(null(), null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error()).context("creating the daemon foreground job");
    }
    let job = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
    let configured = unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of_val(&limits) as u32,
        )
    };
    if configured == 0 {
        return Err(io::Error::last_os_error()).context("configuring the daemon foreground job");
    }

    let mut child = command
        .spawn()
        .context("starting the standalone Corvid daemon")?;
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0 {
        if let Some(status) = child.try_wait()? {
            return Ok(status.code().unwrap_or(1) as u32);
        }
        let error = io::Error::last_os_error();
        let _ = child.kill();
        let _ = child.wait();
        return Err(error).context("assigning the daemon to its foreground job");
    }
    Ok(child.wait()?.code().unwrap_or(1) as u32)
}
