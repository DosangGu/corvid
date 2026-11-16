Corvid v0.2.4 fixes Agent Deck SSH monitoring, Windows window placement, and Desktop terminal colors.

- **SSH hosts and notifications.** Agent Deck discovers verified SSH hosts connected after it opens and shows existing agent runs and unread alerts. Pending questions and approvals can trigger a notification on the first connection. Agent Deck windows share delivery records to avoid duplicate desktop popups. [#100](https://github.com/DosangGu/corvid/pull/100)
- **SSH reconnection.** When the usual command fails for a saved SSH alias, Corvid can detect the remote platform and retry through its managed daemon launcher. [#94](https://github.com/DosangGu/corvid/pull/94)
- **Windows Agent Deck placement.** The mini Deck opens near the notification area on first show and remembers where you moved it when reopened. [#98](https://github.com/DosangGu/corvid/pull/98)
- **Desktop terminal colors.** Desktop renders faint text, and Corvid recognizes colon-separated RGB foreground and background colors. [#99](https://github.com/DosangGu/corvid/pull/99)

To update, open **Settings → About & updates** in Desktop or run `corvid update` for a script installation.

If you are upgrading from a UI-owned companion, stop it once and reconnect Desktop or TUI. The daemon and running agents stay up. [Companion upgrade instructions](https://github.com/DosangGu/corvid/releases/tag/v0.2.3)

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.2.4/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.2.3...v0.2.4)
