use std::env;
use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};
use corvid_core::agent::Provider;
use corvid_daemon::shell_agent;
use corvid_protocol::collector::{HOST_ID, LAUNCH_ASSOCIATION, SERVER_EPOCH, SHELL_TERMINAL};
use corvid_protocol::wire::RegisterShellAgent;
use uuid::Uuid;

pub(crate) fn execute(provider: Provider, argv: Vec<OsString>) -> u8 {
    let name = shell_agent::program(provider);
    let Some(path) = shell_agent::resolve(provider) else {
        eprintln!("{name}: command not found");
        return 127;
    };
    let mut command = Command::new(&path);
    command.env_remove(SHELL_TERMINAL);
    if shell_agent::interactive_invocation(provider, &argv)
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && let Ok((association, observer_args, epoch)) = register(provider, &path, &argv)
    {
        command.env(LAUNCH_ASSOCIATION, association.to_string());
        command.env(SERVER_EPOCH, epoch);
        command.args(observer_args);
    }
    let error = command.args(argv).exec();
    eprintln!("{name}: {error}");
    126
}

fn register(
    provider: Provider,
    path: &Path,
    argv: &[OsString],
) -> Result<(Uuid, Vec<OsString>, String)> {
    let terminal_id = env::var(SHELL_TERMINAL)?;
    let host_id = env::var(HOST_ID)?;
    let runtime = crate::runtime_path(None)?;
    let observer_args = shell_agent::qualified_arguments(provider, path, argv)
        .context("unqualified shell agent")?;
    let registration = RegisterShellAgent {
        terminal_id,
        provider: shell_agent::provider_number(provider),
        executable_path: path.as_os_str().as_encoded_bytes().to_vec(),
    };
    #[cfg(feature = "compio-probe")]
    use corvid_client::{
        compio_host as daemon_host, compio_register_shell_agent as register_agent,
    };
    #[cfg(not(feature = "compio-probe"))]
    use corvid_client::{host as daemon_host, register_shell_agent as register_agent};

    let register = async {
        let current = daemon_host(&runtime).await?;
        ensure!(
            current.host_id == host_id,
            "shell belongs to a different Corvid host"
        );
        let association =
            register_agent(&runtime, &host_id, &current.server_epoch, registration).await?;
        Ok::<_, anyhow::Error>((association, current.server_epoch))
    };
    #[cfg(feature = "compio-probe")]
    let (association, epoch) = compio::runtime::Runtime::new()?.block_on(register)?;
    #[cfg(not(feature = "compio-probe"))]
    let (association, epoch) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(register)?;
    Ok((association, observer_args, epoch))
}
