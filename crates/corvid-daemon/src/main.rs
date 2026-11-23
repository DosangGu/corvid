#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod cli {
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::time::Instant;

    use anyhow::Result;
    use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
    use corvid_client::daemon_control;
    use corvid_daemon::command::{self, hooks};
    use uuid::Uuid;

    #[derive(Parser)]
    #[command(
        name = "corvidd",
        version,
        about = "Run the Corvid PTY and session daemon"
    )]
    struct Options {
        /// Override the private directory containing daemon sockets or named pipes.
        #[arg(long, global = true)]
        runtime_dir: Option<PathBuf>,
        /// Override the private directory containing daemon metadata.
        #[arg(long, global = true)]
        state_dir: Option<PathBuf>,
        #[arg(long, global = true, hide = true)]
        autostarted: bool,
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        #[arg(long, global = true, hide = true)]
        inherit_terminfo: bool,
        #[command(subcommand)]
        command: Option<Command>,
    }

    #[derive(Subcommand)]
    enum Command {
        /// Run the daemon in the foreground. This is the default.
        Daemon,
        /// Start or reuse a background daemon for a local or remote client.
        EnsureDaemon {
            #[cfg(windows)]
            #[arg(long, hide = true)]
            ssh: bool,
        },
        #[command(hide = true)]
        Upgrade {
            #[arg(long)]
            managed_ssh_from: String,
            #[arg(long)]
            expected_host: Option<Uuid>,
            #[cfg(windows)]
            #[arg(long, hide = true)]
            ssh: bool,
        },
        /// Relay the control protocol over stdio.
        Control {
            #[arg(long, required = true)]
            stdio: bool,
        },
        /// Relay the interactive writer protocol over stdio.
        TerminalRelay,
        /// Print the provider observation hooks for explicit installation.
        Hooks {
            #[arg(value_enum)]
            provider: hooks::HookProvider,
        },
        #[command(hide = true)]
        Observe {
            #[arg(long, value_enum)]
            provider: hooks::HookProvider,
            #[arg(long, hide = true)]
            neutral_json: bool,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            #[arg(long, hide = true)]
            claude_input_batch: bool,
        },
    }

    pub fn main() -> ExitCode {
        if let Some(code) = command::run_helper() {
            return code;
        }
        let started = Instant::now();
        match run(parse_options(), started) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("corvidd: {error:#}");
                ExitCode::FAILURE
            }
        }
    }

    fn parse_options() -> Options {
        // Desktop sends the standalone runtime under the pre-existing managed
        // filename. Keep its version banner compatible with older clients.
        let program = std::env::current_exe()
            .ok()
            .filter(|path| path.file_stem() == Some("corvid".as_ref()))
            .map_or("corvidd", |_| "corvid");
        let mut args = Options::command().name(program).get_matches();
        Options::from_arg_matches_mut(&mut args).unwrap_or_else(|error| error.exit())
    }

    fn run(options: Options, started: Instant) -> Result<()> {
        let runtime = options.runtime_dir;
        match options.command {
            None | Some(Command::Daemon) => {
                let runtime = corvid_client::runtime_path(runtime)?;
                let state = daemon_control::state_path(options.state_dir)?;
                command::run_foreground(
                    &runtime,
                    &state,
                    options.autostarted,
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    if options.inherit_terminfo {
                        corvid_daemon::terminfo::Mode::Inherit
                    } else {
                        corvid_daemon::terminfo::Mode::Bundled
                    },
                )
            }
            Some(Command::EnsureDaemon {
                #[cfg(windows)]
                ssh,
            }) => {
                let runtime = corvid_client::runtime_path(runtime)?;
                let state = daemon_control::state_path(options.state_dir)?;
                let executor = compio::runtime::Runtime::new()?;
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                return executor.block_on(daemon_control::unix::ensure(
                    &runtime,
                    Some(&state),
                    |_| Ok(std::env::current_exe()?),
                ));
                #[cfg(windows)]
                executor.block_on(daemon_control::windows::ensure(
                    &runtime,
                    &state,
                    ssh,
                    None,
                    || Ok(std::env::current_exe()?),
                ))
            }
            Some(Command::Upgrade {
                managed_ssh_from,
                expected_host,
                #[cfg(windows)]
                ssh,
            }) => {
                let runtime = corvid_client::runtime_path(runtime)?;
                let executor = compio::runtime::Runtime::new()?;
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                return executor.block_on(daemon_control::unix::upgrade_managed(
                    &runtime,
                    &managed_ssh_from,
                    expected_host,
                ));
                #[cfg(windows)]
                executor.block_on(daemon_control::windows::upgrade_managed(
                    &runtime,
                    &managed_ssh_from,
                    expected_host,
                    ssh,
                ))
            }
            Some(Command::Hooks { provider }) => {
                println!(
                    "{:#}",
                    corvid_daemon::observer_settings(provider.provider())?
                );
                Ok(())
            }
            Some(Command::Observe {
                provider,
                neutral_json,
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                claude_input_batch,
            }) => {
                #[cfg(windows)]
                let claude_input_batch = false;
                hooks::observe(provider, neutral_json, claude_input_batch, runtime, started);
                Ok(())
            }
            Some(Command::Control { stdio: _ }) => {
                let runtime = corvid_client::runtime_path(runtime)?;
                compio::runtime::Runtime::new()?.block_on(corvid_client::compio_relay(&runtime))
            }
            Some(Command::TerminalRelay) => {
                let runtime = corvid_client::runtime_path(runtime)?;
                compio::runtime::Runtime::new()?.block_on(corvid_client::compio_relay_tui(&runtime))
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn main() -> std::process::ExitCode {
    cli::main()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn main() -> std::process::ExitCode {
    eprintln!("corvidd requires Linux, macOS, or Windows");
    std::process::ExitCode::FAILURE
}
