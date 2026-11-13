Corvid v0.1.3 is a patch release for Desktop and TUI on Windows x64 and Linux x86_64.

- Move Agent Deck’s mini deck and management views to GPUI, the UI framework used by Corvid Desktop. The mini deck supports movable and fixed positions, and the Windows tray menu can quit Deck.
- Batch Desktop terminal drawing to reduce rendering overhead when repainting terminal output.
- Refine the Corvid crow icon used across Desktop, the CLI, and Agent Deck.
- Update checksum verification for downloaded updates and remote daemon payloads.

Use `corvid update --check` to check this release and `corvid update` to update a script installation. Desktop provides **Settings → About & updates → Check for updates**. Windows Setup users should run the new Setup. Application updates leave running daemons unchanged; use `corvid upgrade` separately for an automatically started local daemon. Supervised daemons remain managed by their service manager.

Linux Desktop requires X11 or XWayland, Fontconfig, and a Vulkan driver. The packaged build targets Ubuntu 24.04 or compatible newer systems. Linux TUI and daemon binaries use static musl builds. Windows Setup is unsigned. macOS and ARM64 are not supported in this release.

See the [README](https://github.com/DosangGu/corvid/blob/v0.1.3/README.md) and [distribution guide](https://github.com/DosangGu/corvid/blob/v0.1.3/docs/05_release_distribution.md) for installation paths, platform requirements, and update behavior. Archives and Setup include adjacent SHA-256 checksum files; `release-manifest.json` records asset sizes and checksums.

[Full changelog](https://github.com/DosangGu/corvid/compare/v0.1.2...v0.1.3)
