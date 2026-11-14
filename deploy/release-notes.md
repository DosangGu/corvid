Corvid v0.1.5 is a patch release for Desktop and TUI on Windows x64 and Linux x86_64.

- Desktop restores its saved terminal windows and connections on launch. Desktop and TUI remember workspace, tab, and pane selections, and Desktop groups workspaces by machine. ([#47](https://github.com/DosangGu/corvid/pull/47), [#50](https://github.com/DosangGu/corvid/pull/50), [#54](https://github.com/DosangGu/corvid/pull/54))
- Explicitly restore each empty pane's last verified Codex or Claude conversation with **Restore workspace agent sessions** in Desktop or `corvid restore --workspace WORKSPACE`. Existing terminals are reused; failed sessions offer retry and shell recovery. This requires an updated daemon, configured provider hooks, authentication, and the saved conversation on that host. Actual authenticated provider sessions were not exercised for this release; unsupported conversation transitions remain ineligible for automatic recovery. ([#55](https://github.com/DosangGu/corvid/pull/55), [restoration guide](https://github.com/DosangGu/corvid/blob/v0.1.5/docs/02_daemon_foundation.md#restore-saved-agent-conversations))
- Desktop parses terminal output without holding the UI screen/input lock, keeping the previous screen accessible while a large frame is parsed. ([#52](https://github.com/DosangGu/corvid/pull/52))
- Linux terminals enable automatic titles, pane titles stay synchronized, and tab renaming preserves the distinction between a pane's terminal title and a tab's custom name. ([#41](https://github.com/DosangGu/corvid/pull/41), [#42](https://github.com/DosangGu/corvid/pull/42))
- Agent Deck fixes window pinning and restore dragging, and shows the active grid dimensions for its current orientation. ([#45](https://github.com/DosangGu/corvid/pull/45), [#46](https://github.com/DosangGu/corvid/pull/46))

Desktop checks for updates under **Settings → About & updates**. Windows Setup installations from v0.1.4 can install the update there; installations from v0.1.3 or earlier should run the v0.1.5 Setup executable. For script installations, run `corvid update --check` to check for updates or `corvid update` to install. Updating the application does not replace running daemons; use `corvid upgrade` for an automatically started local daemon and the documented host-specific procedure for remote or supervised daemons.

Linux Desktop requires X11 or XWayland, Fontconfig, and a Vulkan driver. The packaged build targets Ubuntu 24.04 or compatible newer systems. Linux TUI and daemon binaries use static musl builds. The Windows Setup executable is unsigned.

The distribution guide covers installation paths, platform requirements, and daemon upgrades. Archives and Setup include SHA-256 checksum files, and `release-manifest.json` lists asset sizes and checksums. [Distribution guide](https://github.com/DosangGu/corvid/blob/v0.1.5/docs/05_release_distribution.md)

[Full changelog](https://github.com/DosangGu/corvid/compare/v0.1.4...v0.1.5)
