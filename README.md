# Corvid

Corvid is a terminal multiplexer for coding agents. A daemon manages
workspaces, tabs, and split panes and keeps shells running when clients detach.
Corvid Desktop opens a local terminal and connects to remote hosts over SSH.
Its Agent Board plugin shows observed Codex and Claude Code activity and opens
the corresponding pane in Desktop.

| Component | Supported platform |
| --- | --- |
| Daemon and TUI | Linux and Windows x64 |
| Corvid Desktop | Windows x64 and Linux with X11 or XWayland |
| Bundled remote daemon | Linux x86_64 and Windows x64 |

## Run the Linux TUI

Install Rust with rustup, a C/C++ toolchain, and the ncurses `tic` compiler.
On Debian or Ubuntu, the system packages are `build-essential` and
`ncurses-bin`. From the repository root, run the following commands in a
terminal.

```sh
cargo build --locked -p corvid-tui
./target/debug/corvid
```

Corvid starts the daemon and opens or reattaches to a shell. Press
<kbd>Ctrl</kbd>+<kbd>]</kbd> to see the available commands. To detach, press
<kbd>Ctrl</kbd>+<kbd>]</kbd>, then <kbd>d</kbd>. Run Corvid again to return to the workspace.
To close the current pane, tab, or session and stop its shells, press
<kbd>Ctrl</kbd>+<kbd>]</kbd> followed by uppercase <kbd>X</kbd>, <kbd>T</kbd>,
or <kbd>S</kbd>, respectively.
See the [daemon guide](docs/02_daemon_foundation.md) for session controls and
systemd setup.

## Run the Windows TUI

From a Windows x64 console with the MSVC Rust toolchain, build and run Corvid.

```powershell
cargo build --locked -p corvid-tui
.\target\debug\corvid.exe
```

Corvid starts its local daemon and opens PowerShell. See the
[Windows daemon guide](docs/02_daemon_foundation.md#run-locally-on-windows)
for local state paths and validation.

## Run Corvid Desktop

Follow the [Windows quick start](docs/04_companion.md#windows-quick-start)
or [Linux Desktop quick start](docs/04_companion.md#linux-desktop-quick-start)
for build requirements. Run this command from the repository root.

```sh
cargo run --locked -p corvid-desktop
```

Desktop starts or reuses the local Corvid daemon and opens a terminal. Open
**Sessions** to open a session under **Local Terminal** or an SSH host.
Use **×** on a session, tab, or pane to close it and stop its shells.
Choose a host from `~/.ssh/config` or enter an alias. Remote hosts require
host-key trust and noninteractive authentication. Pass `-- --ssh-alias work-host`
to launch directly into SSH. The
[Windows packaging guide](docs/04_companion.md#windows-package-and-login-startup)
shows how to build a ZIP with daemon bundles for first-time installation over SSH.

## Development

The root Cargo workspace shares package and dependency versions in `Cargo.toml`
and a single `Cargo.lock`. Run `cargo test --locked` for the default packages.
Use `--workspace` to include Desktop, Deck, and the packaging tool. Full Linux
workspace builds require the [Desktop system packages](docs/04_companion.md#linux-desktop-quick-start).

- [Architecture and roadmap](docs/00_project_design.md)
- [Agent hook coverage and setup](docs/03_provider_qualification.md)
- [Repository guidelines](AGENTS.md)

## License

[Apache License 2.0](LICENSE)
