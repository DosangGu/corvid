Corvid v0.2.5 fixes update detection for managed SSH and WSL daemons.

- Desktop now checks the installed remote Corvid version even when the connection succeeds, and offers the matching bundled update for an older managed installation. [#103](https://github.com/DosangGu/corvid/pull/103)
- Canceling an optional update keeps the working connection. Failed version checks or unavailable or invalid bundles report the problem while retaining that connection.
- Automatic reconnects and PATH installations do not prompt for this update. Equal or newer installations are retained.
- Update dialogs show the installed and bundled versions, verify the target host, and keep the guidance readable on Linux.

To update, open **Settings → About & updates** in Desktop or run `corvid update` for a script installation.

After updating Desktop, explicitly connect to the SSH host or WSL distribution and accept the offered daemon update. Restart existing Claude Code or Codex processes afterward to enable the updated notification collection; running providers retain their previous observer environment.

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.2.5/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.2.4...v0.2.5)
