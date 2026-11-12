Corvid v0.1.0 is the first public packaged release of the terminal multiplexer for coding agents, with Desktop and TUI packages for Windows x64 and Linux x86_64.

- Organize terminals into sessions, tabs, and split panes. Daemon-owned shells keep running when clients detach.
- Desktop includes Linux and Windows daemon payloads for local sessions, SSH setup, and Windows-to-WSL connections.
- The built-in Agents window tracks observed Codex and Claude Code activity and opens the corresponding terminal pane. Its floating Agent Deck offers configurable landscape and portrait grids; Windows also provides tray integration and notifications.
- Windows x64 Setup includes Desktop and Agent Deck. Update it with a new Setup.
- Install or update the Linux TUI with `install.sh`; Windows has `install.ps1`. Add `--product desktop` or `-Product desktop` to install Desktop.
- Use `corvid update --check` to check GitHub Releases and `corvid update` to update a script installation. Desktop provides **Settings → About & updates → Check for updates**, with background installation and a choice to restart now or later.
- Updates preserve older executables and leave running daemons unchanged. `corvid upgrade` separately replaces automatically started daemons using the existing terminal handoff protocol. Supervised daemons remain managed by their service manager.

Linux Desktop requires X11 or XWayland, Fontconfig, and a Vulkan driver. The packaged build targets Ubuntu 24.04 or compatible newer systems. Linux TUI and daemon binaries use static musl builds. Windows Setup is unsigned. macOS and ARM64 are not supported in this release.

See the [README](https://github.com/DosangGu/corvid/blob/v0.1.0/README.md) and [distribution guide](https://github.com/DosangGu/corvid/blob/v0.1.0/docs/05_release_distribution.md) for installation paths, platform requirements, and update behavior. Archives and Setup include adjacent SHA-256 checksum files; `release-manifest.json` records asset sizes and checksums.
