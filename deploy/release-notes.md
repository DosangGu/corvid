Corvid v0.3.7 fixes Desktop restarts after updates, Agent Deck reuse across hosts, and TUI scrollback interactions.

- Desktop restarts after applying an update even if the window that started the update has closed. [#233](https://github.com/DosangGu/corvid/pull/233)
- Desktop reuses a single Agent Deck across host selections. On Unix, updates close duplicate Deck windows from older versions. [#229](https://github.com/DosangGu/corvid/pull/229) [#232](https://github.com/DosangGu/corvid/pull/232)
- The TUI handles history navigation and copy commands during wheel-driven scrollback loading. It also reads the final terminal status when a render acknowledgement overlaps shutdown. [#227](https://github.com/DosangGu/corvid/pull/227) [#228](https://github.com/DosangGu/corvid/pull/228)
- Windows retries transient file-sharing errors and allows more time for concurrent view-state writes to complete. [#223](https://github.com/DosangGu/corvid/pull/223) [#225](https://github.com/DosangGu/corvid/pull/225)

To update, open **Settings → About & updates** in Desktop or run `corvid update` for a script installation.

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.3.7/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.3.6...v0.3.7)
