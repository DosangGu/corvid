# Corvid

[![Crates.io](https://img.shields.io/crates/v/corvid-tui.svg)](https://crates.io/crates/corvid-tui)
[![GitHub Release](https://img.shields.io/github/v/release/DosangGu/corvid)](https://github.com/DosangGu/corvid/releases/latest)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](https://github.com/DosangGu/corvid/blob/main/LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.100%2B-orange.svg)](https://github.com/DosangGu/corvid/blob/main/rust-toolchain.toml)

Corvid is a terminal multiplexer for coding agents. A background daemon manages
workspaces, tabs, and split panes and keeps shells running when clients detach.
**Corvid Desktop** opens a local terminal and connects to remote hosts over SSH,
while its built-in **Corvid Agent Deck** window (with Windows system tray and
macOS menu-bar support) tracks **Claude Code**, **Codex CLI**, and
**Antigravity CLI** activity and focuses the exact pane on click. Completed
agents remain visible until their panes close, and additional runs queue by
priority. See the [Agent Deck guide](https://github.com/DosangGu/corvid/blob/main/docs/guides/agent-deck.md).

## Supported Platforms

| Component | Supported platforms |
| --- | --- |
| Daemon and TUI | Linux and Windows x64/ARM64; macOS 15+ Apple Silicon |
| Corvid Desktop | Windows x64/ARM64; Linux x64/ARM64 with X11 or XWayland; macOS 15+ Apple Silicon |
| Bundled remote daemon | Linux x64/ARM64, Windows x64/ARM64, macOS Apple Silicon |

Physical ARM64 GPU rendering and interactive Windows ConPTY behavior require
separate device qualification.

## Install

### TUI and Daemon (Linux, WSL, macOS Apple Silicon)

```sh
curl -fsSL https://github.com/DosangGu/corvid/releases/latest/download/install.sh | sh
```

Re-running the installer updates the existing installation. Use
`corvid update --check` or `corvid update` to update a script installation.

### Cargo (crates.io)

```sh
cargo install --locked corvid-tui
```

See [Cargo installation requirements](https://github.com/DosangGu/corvid/blob/main/docs/guides/release-and-install.md#install-with-cargo)
for build prerequisites (`tic`, Zig, and C toolchain).

### Corvid Desktop

Download Windows, Linux, and macOS packages from
[GitHub Releases](https://github.com/DosangGu/corvid/releases):

- **Windows**: [Windows Setup](https://github.com/DosangGu/corvid/blob/main/docs/guides/release-and-install.md#windows-setup)
- **macOS**: [Unsigned macOS DMG](https://github.com/DosangGu/corvid/blob/main/docs/guides/release-and-install.md#install-on-macos)
- **Updates**: Desktop provides **Settings → About & updates → Check for updates**. Package updates leave running daemons unchanged; use `corvid upgrade` separately for managed daemons.

See the [Release and Install guide](https://github.com/DosangGu/corvid/blob/main/docs/guides/release-and-install.md)
for custom paths, system requirements, and daemon lifecycle details.

## Quick Start

### TUI

```sh
corvid               # Start in current directory or reattach to last workspace
corvid list          # List workspace IDs and names
corvid attach review # Attach to an existing workspace
corvid new review    # Create a workspace rooted in the current directory
```

Press <kbd>Ctrl</kbd>+<kbd>]</kbd> for the command prefix:

- `d`: detach (sessions stay alive in the daemon)
- `w`: switch workspaces
- `t`: manage tabs
- `p`: select, split, or close panes
- `?`: search commands

See the [Daemon guide](https://github.com/DosangGu/corvid/blob/main/docs/guides/daemon.md)
for session controls, SSH response notifications, and `systemd` setup.

### Corvid Desktop

Desktop starts or reuses the local Corvid daemon and opens a terminal. Open
**Settings → Connections** to choose **Local Terminal** or an SSH host from
`~/.ssh/config` (or pass `-- --ssh-alias work-host` at launch). Remote hosts
require host-key trust and noninteractive authentication. Use **×** on a
session, tab, or pane to close it and stop its shells.

## Build from Source

Install Rust (`rustup`), a C/C++ toolchain, and ncurses `tic` (`build-essential`
and `ncurses-bin` on Debian/Ubuntu).

```sh
# Build and run the TUI
cargo build --locked -p corvid-tui
./target/debug/corvid
```

On Windows (x64 MSVC toolchain):

```powershell
cargo build --locked -p corvid-tui
.\target\debug\corvid.exe
```

To build and run Corvid Desktop + Agent Deck (see the
[Linux](https://github.com/DosangGu/corvid/blob/main/docs/guides/desktop.md#linux-desktop-quick-start),
[Windows](https://github.com/DosangGu/corvid/blob/main/docs/guides/desktop.md#windows-quick-start),
or [macOS](https://github.com/DosangGu/corvid/blob/main/docs/guides/desktop.md#macos)
guides for platform prerequisites):

```sh
cargo build --locked -p corvid-agent-deck-ui
cargo run --locked -p corvid-desktop
```

## Documentation & Contributing

- [Documentation index](https://github.com/DosangGu/corvid/blob/main/docs/README.md) — Audience-oriented map of all guides and specs
- [Architecture overview](https://github.com/DosangGu/corvid/blob/main/docs/architecture/overview.md) — State ownership, identity, slots, and roadmap
- [Provider hooks and observers](https://github.com/DosangGu/corvid/blob/main/docs/guides/providers.md) — Claude Code, Codex CLI, and Antigravity CLI setup
- [Release distribution and updates](https://github.com/DosangGu/corvid/blob/main/docs/guides/release-and-install.md) — Installers, Cargo, and publishing
- [CI and validation](https://github.com/DosangGu/corvid/blob/main/docs/contributing/ci-and-validation.md) — Platform coverage, local UI/TTY/SSH tests, and Dependabot policy
- [Workflow, PRs, and TODOs](https://github.com/DosangGu/corvid/blob/main/docs/contributing/workflow-and-prs.md) — Worktree workflow, commit conventions, and TODO-to-issue format
- [Repository guidelines (`AGENTS.md`)](https://github.com/DosangGu/corvid/blob/main/AGENTS.md) — Workspace crates, core invariants, and agent task router

## License

Licensed under the [Apache License 2.0](https://github.com/DosangGu/corvid/blob/main/LICENSE).
