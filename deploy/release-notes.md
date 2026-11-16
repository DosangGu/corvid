Corvid v0.2.3 keeps agent monitoring running even when Agent Deck is closed.

- **Automatic agent monitoring:** Desktop and TUI start a background monitor after connecting to a host. It continues tracking agents after the client closes. The monitor reuses the verified SSH or WSL connection settings, and Agent Deck and the USB Stream Deck adapter read the same status. [#95](https://github.com/DosangGu/corvid/pull/95)
- **Upgrading an existing monitor:** Quit the old Agent Deck, then stop its `corvid-agent-deck --companion-service` process if it is still running. If you started the old companion with `corvid companion run`, stop that process instead. Reconnect Desktop or TUI to start the new monitor. The daemon and running agents stay up.

To update, open **Settings → About & updates** in Desktop or run `corvid update` for a script installation.

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.2.3/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.2.2...v0.2.3)
