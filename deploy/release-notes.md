Corvid v0.3.2 improves Desktop terminal navigation, Agent Deck focus, and WSL startup.

- Scroll the Desktop tab strip to reach overflowed tabs. **Ctrl+Shift+W** closes the active pane in a split. [#149](https://github.com/DosangGu/corvid/pull/149) · [#148](https://github.com/DosangGu/corvid/pull/148)
- Use **Ctrl+Shift+F** to search Desktop terminal history. The wheel follows the active terminal screen and mouse mode, and scrollback retains terminal colors and styles. [#150](https://github.com/DosangGu/corvid/pull/150) · [#151](https://github.com/DosangGu/corvid/pull/151)
- **Ctrl+click** a terminal URL to open it in its registered local app. On macOS, **Cmd+click** also works. [#154](https://github.com/DosangGu/corvid/pull/154)
- Open an agent's pane from Agent Deck across local, SSH, and WSL Desktop connections. On Windows and macOS, clicking an individual agent notification also brings its pane forward. Agent Deck retires stale runs when a daemon is replaced. [Host focus](https://github.com/DosangGu/corvid/commit/6af8c768610fbf6a12772b8d8807b029de923628) · [#153](https://github.com/DosangGu/corvid/pull/153) · [#152](https://github.com/DosangGu/corvid/pull/152)
- WSL startup uses a private runtime directory when the WSLg runtime path fails the daemon's permission checks. [#156](https://github.com/DosangGu/corvid/pull/156)

To update, open **Settings → About & updates** in Desktop or run `corvid update` for a script installation. When connecting to an older managed SSH host or WSL distribution, accept the offered daemon update.

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.3.2/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.3.1...v0.3.2)
