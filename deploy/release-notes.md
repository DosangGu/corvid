Corvid now runs natively on Apple Silicon Macs, Linux ARM64, and Windows 11 on ARM64. Desktop also gains terminal font selection, clearer terminal artwork, and more responsive tab titles.

- **macOS 15+ on Apple Silicon:** Install Corvid Desktop from the DMG or install the TUI with `install.sh`. Desktop includes Agent Deck, with a menu bar item and native notifications. Intel Macs are outside this release's support scope. [#62](https://github.com/DosangGu/corvid/pull/62)
- **Linux and Windows ARM64:** Desktop and TUI ship as native ARM64 packages, alongside the existing x64 builds. Windows 11 on ARM64 has its own Setup executable. Desktop can also provision matching daemons on supported ARM64 hosts over SSH, and in ARM64 WSL distributions. [#73](https://github.com/DosangGu/corvid/pull/73)
- **Desktop terminal fonts and artwork:** Choose an installed monospaced font in **Settings → Terminal**. The choice persists across launches. Box drawing, block elements, and Braille art draw directly on the terminal grid, so adjacent shapes line up. [#64](https://github.com/DosangGu/corvid/pull/64)
- **More responsive tab titles:** Desktop keeps automatic titles up to date during bursts of terminal output and restores the last title when reconnecting. [#72](https://github.com/DosangGu/corvid/pull/72)

### Install or update

- **macOS:** Open the DMG and drag **Corvid.app** to Applications. The app is ad-hoc signed and is not notarized. If Gatekeeper blocks the first launch, see the [macOS installation guide](https://github.com/DosangGu/corvid/blob/v0.2.0/docs/07_macos.md#install).
- **Existing installations:** Open **Settings → About & updates** in Desktop, or run `corvid update` for a script installation. If you installed Windows Desktop with Setup v0.1.3 or earlier, run the new Setup executable for your architecture. Windows Setup is unsigned.
- **Linux Desktop:** The package targets Ubuntu 24.04 or compatible newer systems and requires X11 or XWayland, Fontconfig, and a Vulkan driver.

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.2.0/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.1.6...v0.2.0)
