Corvid v0.2.2 improves Agent Deck positioning on Windows and keeps running Decks up to date when Desktop restarts after an update.

- **Agent Deck updates on Windows:** After a Desktop update, restarting Desktop replaces older running Decks in its saved controller scopes with the bundled version. Hidden Decks stay hidden, and stopped Decks stay stopped. [#90](https://github.com/DosangGu/corvid/pull/90)
- **Agent Deck positioning on Windows:** Deck opens near the system tray by default. Drag an unpinned Deck near a monitor's work-area edge to snap it into place in either the compact or expanded view. [#91](https://github.com/DosangGu/corvid/pull/91)

To update, open **Settings → About & updates** in Desktop or run `corvid update` for a script installation.

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.2.2/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.2.1...v0.2.2)
