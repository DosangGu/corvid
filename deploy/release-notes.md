- Push dirty visible pane updates from the daemon (`SetPreviewTab` and unsolicited `PanePreviewFrame` streams) and unify Desktop's screen and preview state into a bounded `pane_screens` LRU cache so unfocused panes stay live and pane/tab switches do not flash blank. [#545](https://github.com/DosangGu/corvid/pull/545)
- Eliminate TUI pane and tab switch flicker by replacing full-screen clears (`\x1b[2J`) with row-overwrite redraws and wrapping outer TTY frames in DEC Synchronized Output (`\x1b[?2026h`…`\x1b[?2026l`). [#552](https://github.com/DosangGu/corvid/pull/552)
- Unify Agent Deck host sync around a single persistent companion connection per host with event-driven `WaitPresentationChange` notifications, resilient multi-page sync, offline SSH reconnects, and `Blocked` status surfacing. [#553](https://github.com/DosangGu/corvid/pull/553)
- Replace Agent Deck third-party provider logo SVGs with minimal monochrome status icons. [#551](https://github.com/DosangGu/corvid/pull/551)

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.9.0/docs/guides/release-and-install.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.8.3...v0.9.0)
