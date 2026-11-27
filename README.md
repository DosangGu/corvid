# Corvid

Corvid is a terminal multiplexer for coding agents. A daemon manages
workspaces, tabs, and split panes and keeps shells running when clients detach.
Corvid Desktop opens a local terminal and connects to remote hosts over SSH.
Its built-in Agents window (Corvid Agent Deck) shows observed Codex and Claude Code
activity and opens the corresponding pane in Desktop. Agent Deck provides a Windows system tray
and macOS menu-bar item with native notifications. Completed agents remain until their panes close;
additional agents queue and can move into view as their priority changes. See [Agent Deck](docs/guides/agent-deck.md).

| Component | Supported platform |
| --- | --- |
| Daemon and TUI | Linux and Windows x64/ARM64; macOS 15+ Apple Silicon |
| Corvid Desktop | Windows x64/ARM64; Linux x64/ARM64 with X11 or XWayland; macOS 15+ Apple Silicon |
| Bundled remote daemon | Linux x64/ARM64, Windows x64/ARM64, macOS Apple Silicon |

Linux and Windows ARM64 assets are available from builds of this revision and
future releases. Older releases do not contain these assets. Physical ARM64 GPU rendering
and interactive ConPTY behavior require separate qualification.

## Install a release

Download Windows, Linux, and macOS Desktop or TUI packages from
[GitHub Releases](https://github.com/DosangGu/corvid/releases).
Desktop offers [Windows Setup](docs/guides/release-and-install.md#windows-setup) and an
[unsigned macOS DMG](docs/guides/release-and-install.md#install-on-macos).
On Linux x64/ARM64, WSL, or macOS Apple Silicon, install the TUI with:

```sh
curl -fsSL https://github.com/DosangGu/corvid/releases/latest/download/install.sh | sh
```

Re-running the installer updates the same installation. Once installed, use
`corvid update --check` to check for a release or `corvid update` to update a script installation.
Desktop provides **Settings → About & updates → Check for updates**. Package updates leave running
daemons unchanged; use `corvid upgrade` separately for automatically started daemons.

For versions published to crates.io, install or update the TUI with
`cargo install --locked corvid-tui` and run `corvid`. See the
[Cargo installation requirements](docs/guides/release-and-install.md#install-with-cargo).

See the [distribution guide](docs/guides/release-and-install.md) for Desktop and
Windows installation, custom paths, system requirements, and daemon lifecycle.

## Build and run the Linux TUI

Install Rust with rustup, a C/C++ toolchain, and the ncurses `tic` compiler.
On Debian or Ubuntu, the system packages are `build-essential` and
`ncurses-bin`. From the repository root, run the following commands in a
terminal.

```sh
cargo build --locked -p corvid-tui
./target/debug/corvid
```

From an installed CLI, run `corvid` to start in the current directory or
return to your last workspace. Run `corvid list` to see workspace IDs
and names, `corvid attach review` to open an existing workspace, or
`corvid new review` to create a workspace rooted in your current directory.

Press
<kbd>Ctrl</kbd>+<kbd>]</kbd> to see the available commands. To detach, press
<kbd>Ctrl</kbd>+<kbd>]</kbd>, then <kbd>d</kbd>. Run Corvid again to return to the workspace.
Use `Ctrl+]` then `w` to switch workspaces, `t` to manage tabs, or `p`
to select, split or close a pane. Use `Ctrl+]` then `?` to search commands.
See the [daemon guide](docs/guides/daemon.md) for session controls and
systemd setup.

## Run the Windows TUI

From a Windows x64 console with the MSVC Rust toolchain, build and run Corvid.

```powershell
cargo build --locked -p corvid-tui
.\target\debug\corvid.exe
```

Corvid starts its local daemon and opens PowerShell. See the
[Windows daemon guide](docs/guides/daemon.md#run-locally-on-windows)
for local state paths and validation.

## Run Corvid Desktop

Follow the [Windows quick start](docs/guides/desktop.md#windows-quick-start),
[Linux Desktop quick start](docs/guides/desktop.md#linux-desktop-quick-start),
or [macOS guide](docs/guides/desktop.md#macos)
for build requirements. Build the Agents window and run Desktop from the repository root.

```sh
cargo build --locked -p corvid-agent-deck-ui
cargo run --locked -p corvid-desktop
```

Desktop starts or reuses the local Corvid daemon and opens a terminal. Open
**Settings → Connections** to choose **Local Terminal** or an SSH host.
Use **×** on a session, tab, or pane to close it and stop its shells.
Choose a host from `~/.ssh/config` or enter an alias. Remote hosts require
host-key trust and noninteractive authentication. Pass `-- --ssh-alias work-host`
to launch directly into SSH. The
[Windows packaging guide](docs/guides/desktop.md#windows-package-and-login-startup)
shows how to build a ZIP with daemon bundles for first-time installation over SSH.

## Development

The root Cargo workspace shares package and dependency versions in `Cargo.toml`
and a single `Cargo.lock`. Run `cargo test --locked` for the default packages.
Use `--workspace` to include Desktop, Deck, and the packaging tool. Full Linux
workspace builds require the [Desktop system packages](docs/guides/desktop.md#linux-desktop-quick-start).

See [CI validation](docs/contributing/ci-and-validation.md) for platform coverage, real TTY/SSH
and Desktop UI tests, local reproduction commands, and remaining device checks.

[Dependabot](.github/dependabot.yml) checks Cargo dependencies and GitHub Actions
every day (including weekends) at 09:00 Asia/Seoul, with up to five open
version-update PRs per ecosystem. Version updates wait at least 14 days after a
release and become eligible at the next check. This cooldown does not delay
security updates and cannot be configured by vulnerability severity. Protobuf
dependencies (`prost*` and `protox`) are updated together; all other updates,
including patches, receive individual PRs for review.
Dependency PRs run the existing packaging and workspace CI and require review
before manual merging. Dependabot security updates are controlled separately
in the repository's security settings.

- [Documentation index](docs/README.md)
- [Architecture and roadmap](docs/architecture/overview.md)
- [Agent hook coverage and setup](docs/guides/providers.md)
- [Release distribution and updates](docs/guides/release-and-install.md)
- [Repository guidelines](AGENTS.md)

### TODO issues

The [TODO to Issue workflow](.github/workflows/todo-to-issue.yml) uses
[alstr/todo-to-issue-action](https://github.com/alstr/todo-to-issue-action) to create
GitHub issues from TODO comments added by pushes to `main`, including merged PRs.
Only the text on the `TODO:` line becomes the issue title; following comment
lines become its body. Keep a short, unique, self-contained title on one physical
line. Shorten a long title instead of wrapping it, and start any explanation on
the next comment line. The explanation may wrap normally. For example:

```rust
// TODO: Isolate Desktop ANSI parsing from the UI lock
// Publish a completed screen snapshot so large frames cannot block UI/input reads.
```

Removing a TODO attempts to close its matching issue. Use unique TODO titles:
without an explicit `Issue URL:` comment, the action matches issues by title.
Preserve an existing `Issue URL:` line when editing a TODO so a title change
stays linked to the same issue.
The workflow does not insert URLs or commit changes. It ignores Markdown files
and processes only the pushed diff, so pre-existing TODOs are not backfilled.
It uses the built-in `GITHUB_TOKEN`; no additional secret is required.

## License

[Apache License 2.0](LICENSE)
