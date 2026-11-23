//! Process entry points shared by corvidd and the legacy Corvid CLI.

pub mod hooks;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod shell_agent;

use std::ffi::OsStr;
use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;

/// Dispatch terminal hosts and provider wrappers before parsing CLI options.
pub fn run_helper() -> Option<ExitCode> {
    let mut args = std::env::args_os();
    let invoked_as = args.next()?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Some(provider) = crate::shell_agent::invoked_provider(&invoked_as) {
        return Some(ExitCode::from(shell_agent::execute(
            provider,
            args.collect(),
        )));
    }
    #[cfg(windows)]
    let _ = invoked_as;

    if args.next().as_deref() != Some(OsStr::new(crate::terminal::HOST_COMMAND)) {
        return None;
    }

    Some(match crate::terminal::run_helper(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("corvid terminal host: {error:#}");
            ExitCode::FAILURE
        }
    })
}

/// Serve a runtime until it stops or receives an OS shutdown signal.
pub fn run_foreground(runtime: &Path, state: &Path, autostarted: bool) -> Result<()> {
    let executor = compio::runtime::Runtime::new()?;
    executor.block_on(async {
        let (stop, stopped) = corvid_async::oneshot::channel::<()>();
        let daemon = crate::run_native(runtime, state, autostarted, async move {
            let _ = stopped.await;
        });
        let mut daemon = std::pin::pin!(daemon);
        let signal = async {
            #[cfg(unix)]
            corvid_async::select! {
                result = compio::signal::unix::signal(nix::libc::SIGTERM) => result,
                result = compio::signal::ctrl_c() => result,
            }
            #[cfg(windows)]
            compio::signal::ctrl_c().await
        };
        corvid_async::select! {
            biased;
            result = signal => {
                result?;
                drop(stop);
                daemon.await?;
            }
            result = &mut daemon => result?,
        }
        Ok(())
    })
}
