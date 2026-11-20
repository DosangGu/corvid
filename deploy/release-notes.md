Corvid v0.3.6 preserves existing macOS Dock items across managed Desktop updates and restores Windows title bar gestures.

- macOS script installations keep the same app bundle across updates. The first update migrates the existing bundle to a permanent location so Finder bookmarks and pinned Dock items keep working. Running sessions continue through daemon handoff. [#203](https://github.com/DosangGu/corvid/pull/203)
- Windows Desktop restores title bar dragging, double-click to maximize or restore, and native caption handling around the tabs. [#202](https://github.com/DosangGu/corvid/pull/202)
- Corvid now builds and verifies the TUI and its dependencies as Cargo source packages on Linux, Windows, and macOS. This release is available through GitHub Releases. [#200](https://github.com/DosangGu/corvid/pull/200)

To update, open **Settings → About & updates** in Desktop or run `corvid update` for a script installation.

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.3.6/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.3.5...v0.3.6)
