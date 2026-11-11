#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> anyhow::Result<std::process::ExitCode> {
    corvid_update::launch(true)
}
