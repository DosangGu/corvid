Corvid v0.1.6 is a patch release for Desktop and TUI on Windows x64 and Linux x86_64.

- Desktop fixes the `UnsupportedCapability` connection error after updating while an older daemon is still running. When Desktop connects to an older local daemon that supports live terminal handoff, it upgrades the daemon and preserves running shells. If handoff is unavailable, Desktop can still connect using the daemon's supported capabilities. SSH and WSL updates follow the host-specific setup flow. [#59](https://github.com/DosangGu/corvid/pull/59)
- Desktop uses a single animated loading indicator while checking for or preparing an update. The panel shows success, retry, or restart actions when the operation finishes. [#60](https://github.com/DosangGu/corvid/pull/60)

Open **Settings → About & updates** to update Desktop. Windows Setup installations from v0.1.4 onward can install the update there. For Windows Setup v0.1.3 or earlier, run the v0.1.6 Setup executable. Script installations can update with `corvid update`.

Linux Desktop requires X11 or XWayland, Fontconfig, and a Vulkan driver. The packaged build targets Ubuntu 24.04 or compatible newer systems. Linux TUI and daemon binaries use static musl builds. The Windows Setup executable is unsigned. Archives and Setup include SHA-256 checksum files. [Distribution guide](https://github.com/DosangGu/corvid/blob/v0.1.6/docs/05_release_distribution.md)

[Full changelog](https://github.com/DosangGu/corvid/compare/v0.1.5...v0.1.6)
