Corvid v0.1.4 is a patch release for Desktop and TUI on Windows x64 and Linux x86_64.

- Windows Setup installations can download and install updates directly from Desktop. After Setup completes, choose **Restart to apply** to open the new version. Running terminal jobs continue through the update. The `corvid update` command also supports Windows Setup installations. ([#36](https://github.com/DosangGu/corvid/pull/36))
- The experimental Windows Stream Deck adapter now handles USB HID reports directly for supported 15-key devices. The adapter is available from source. ([#38](https://github.com/DosangGu/corvid/pull/38))

If you installed v0.1.3 with Windows Setup, run the v0.1.4 Setup executable to get the new updater. For script installations, run `corvid update --check` to check for updates or `corvid update` to install. Desktop also checks for updates under **Settings → About & updates**.

Linux Desktop requires X11 or XWayland, Fontconfig, and a Vulkan driver. The packaged build targets Ubuntu 24.04 or compatible newer systems. Linux TUI and daemon binaries use static musl builds. The Windows Setup executable is unsigned.

The distribution guide covers installation paths, platform requirements, and daemon upgrades. Archives and Setup include SHA-256 checksum files, and `release-manifest.json` lists asset sizes and checksums. [Distribution guide](https://github.com/DosangGu/corvid/blob/v0.1.4/docs/05_release_distribution.md)

[Full changelog](https://github.com/DosangGu/corvid/compare/v0.1.3...v0.1.4)
