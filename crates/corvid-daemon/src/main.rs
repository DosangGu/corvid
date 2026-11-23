#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod cli {
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::time::Instant;

    use anyhow::Result;
    use clap::{Parser, Subcommand};
    use corvid_daemon::command::{self, hooks};
    use corvid_protocol::paths::{StateRole, state_directory};

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
        #[command(subcommand)]
        command: Option<Command>,
    }

    #[derive(Subcommand)]
    enum Command {
        /// Run the daemon in the foreground. This is the default.
        Daemon,
        /// Relay the control protocol over stdio.
        Control {
            #[arg(long, required = true)]
            stdio: bool,
        },
        /// Relay the interactive writer protocol over stdio.
        TerminalRelay,
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
        match run(Options::parse(), started) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("corvidd: {error:#}");
                ExitCode::FAILURE
            }
        }
    }

    fn run(options: Options, started: Instant) -> Result<()> {
        let runtime = options.runtime_dir;
        match options.command {
            None | Some(Command::Daemon) => {
                let runtime = corvid_client::runtime_path(runtime)?;
                let state = state_directory(StateRole::Daemon, options.state_dir)?;
                command::run_foreground(
                    &runtime,
                    &state,
                    options.autostarted,
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    corvid_daemon::terminfo::Mode::Bundled,
                )
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
