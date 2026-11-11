#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> anyhow::Result<std::process::ExitCode> {
    corvid_update::launch("plugins/agent-deck/corvid-agent-deck.exe", false)
}
