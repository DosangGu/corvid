Corvid v0.1.2 is a patch release for Desktop and TUI on Windows x64 and Linux x86_64.

- Preserve the configured shell locale when bootstrapping SSH and WSL daemons, preventing Unicode input from inheriting the transport's temporary C locale.
- Remove Observe mode and read-only attachment from Desktop and TUI. Opening an existing pane or running `corvid attach` resumes it interactively, transferring input ownership from the previous client while the shell keeps running. Older daemons may require `--takeover` or an upgrade.
- Apply the shared Corvid icon to Windows executables, Setup, shortcuts, and Linux Desktop windows and application menu entries.
- Update runtime dependencies and packaging workflow actions, and use the exact version tag as the GitHub Release title.

Use `corvid update --check` to check this release and `corvid update` to update a script installation. Desktop provides **Settings → About & updates → Check for updates**. Windows Setup users should run the new Setup. Application updates leave running daemons unchanged; use `corvid upgrade` separately for an automatically started local daemon. Supervised daemons remain managed by their service manager.

Linux Desktop requires X11 or XWayland, Fontconfig, and a Vulkan driver. The packaged build targets Ubuntu 24.04 or compatible newer systems. Linux TUI and daemon binaries use static musl builds. Windows Setup is unsigned. macOS and ARM64 are not supported in this release.

See the [README](https://github.com/DosangGu/corvid/blob/v0.1.2/README.md) and [distribution guide](https://github.com/DosangGu/corvid/blob/v0.1.2/docs/05_release_distribution.md) for installation paths, platform requirements, and update behavior. Archives and Setup include adjacent SHA-256 checksum files; `release-manifest.json` records asset sizes and checksums.

[Full changelog](https://github.com/DosangGu/corvid/compare/v0.1.1...v0.1.2)
