Corvid's first packaged release includes the Desktop terminal workspace and TUI for Windows x64 and Linux x86_64.

- Desktop includes Linux and Windows daemon payloads for local sessions, SSH setup, and Windows-to-WSL connections.
- Windows x64 Setup includes Desktop and Agent Deck. Update it with a new Setup.
- Install or update the Linux TUI with `install.sh`; Windows has `install.ps1`. Add `--product desktop` or `-Product desktop` to install Desktop.
- Use `corvid update --check` to check GitHub Releases and `corvid update` to update a script installation. Desktop provides **Help → Check for updates**.
- Updates preserve older executables and leave running daemons unchanged. `corvid upgrade` separately replaces automatically started daemons using the existing terminal handoff protocol. Supervised daemons remain managed by their service manager.

Linux Desktop requires X11 or XWayland, Fontconfig, and a Vulkan driver. The packaged build targets Ubuntu 24.04 or compatible newer systems. Linux TUI and daemon binaries use static musl builds.

See the repository README and `docs/05_release_distribution.md` for installation paths, platform requirements, and update behavior. macOS and ARM64 are not supported in this release.
