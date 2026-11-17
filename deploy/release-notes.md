Corvid v0.3.0 adds workspace directory selection, independent terminal text sizing, and a multi-host Agent Deck.

- Choose a working directory when creating a workspace in Desktop. [#105](https://github.com/DosangGu/corvid/pull/105)
- Adjust the app scale and terminal font size independently in Appearance settings. Ctrl+=, Ctrl+-, and Ctrl+0 resize terminal text. Use Cmd on macOS. [#102](https://github.com/DosangGu/corvid/pull/102)
- See agents from recognized local and SSH hosts in one Deck, including hosts connected after Deck starts. Cards show agent logos and status colors. [#110](https://github.com/DosangGu/corvid/pull/110), [#125](https://github.com/DosangGu/corvid/pull/125)
- Agent Deck cards focus the exact Desktop pane, including restoring a minimized Desktop. Windows agent notifications open the Deck grid. Agent Deck and physical Stream Deck buttons share the host's Desktop focus service. [#123](https://github.com/DosangGu/corvid/pull/123), [#124](https://github.com/DosangGu/corvid/pull/124), [#129](https://github.com/DosangGu/corvid/pull/129)
- Retain agent monitoring across daemon upgrades and Deck state when reopening from the tray. Desktop updates also refresh running Agent Decks on Linux and macOS. [#117](https://github.com/DosangGu/corvid/pull/117), [#122](https://github.com/DosangGu/corvid/pull/122), [#130](https://github.com/DosangGu/corvid/pull/130)
- End the active pane with Ctrl+Shift+X or the active tab with Ctrl+Shift+W. Desktop releases its writer before confirming detach so a rapid relaunch can reconnect. [#128](https://github.com/DosangGu/corvid/pull/128), [#131](https://github.com/DosangGu/corvid/pull/131)
- Coalesce repeated response observations within the same agent turn to reduce duplicate attention notices. [#126](https://github.com/DosangGu/corvid/pull/126)

To update, open **Settings → About & updates** in Desktop or run `corvid update` for a script installation. When connecting to an older managed SSH host or WSL distribution, accept the offered daemon update.

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.3.0/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.2.5...v0.3.0)
