Corvid v0.2.1 fixes Agent Deck dragging and intermittent failures when saving views on Windows, and recognizes more Claude Code and Codex CLI versions on Linux and macOS.

- **Agent Deck dragging on Windows:** Drag the title area to move an unpinned Agent Deck, in either the compact or expanded view. Pinning keeps the window fixed and on top. [#89](https://github.com/DosangGu/corvid/pull/89)
- **Windows view saving:** When two clients save their views at the same time, Corvid briefly waits for the other save to finish so the view can be saved and restored on the next launch. [#83](https://github.com/DosangGu/corvid/pull/83)
- **Agent observation on Linux and macOS:** Corvid recognizes Claude Code and Codex CLI by their version banners without requiring an exact version match. Observation still depends on the CLI providing the supported hooks. [#84](https://github.com/DosangGu/corvid/pull/84)

To update, open **Settings → About & updates** in Desktop or run `corvid update` for a script installation.

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.2.1/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.2.0...v0.2.1)
