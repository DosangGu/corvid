Corvid v0.4.0 moves Desktop and Agent Deck to GPUI CE and retires the experimental Stream Deck integration.

- Desktop and the built-in Agent Deck now use GPUI CE 0.3.0. On Linux, Desktop continues to use X11 or XWayland with Vulkan. [#249](https://github.com/DosangGu/corvid/pull/249)
- Windows and macOS packages drop the experimental Deck app and its USB adapter. Corvid Agent Deck remains included with Desktop and supports WezTerm pairing. [#250](https://github.com/DosangGu/corvid/pull/250)
- Building Corvid from source now requires Rust 1.100 or newer. [#236](https://github.com/DosangGu/corvid/pull/236)

To update, open **Settings → About & updates** in Desktop or run `corvid update` for a script installation.

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.4.0/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.3.7...v0.4.0)
