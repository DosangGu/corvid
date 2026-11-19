Corvid v0.3.5 improves SSH session recovery, preserves running sessions during updates, and makes Desktop terminals easier to read and navigate.

- SSH clients restore input to attached terminals after reconnecting and stop automatic retries after a prolonged outage. When an old terminal cannot be restored, Corvid can resume a verified saved agent conversation or offer recovery choices before opening a shell. [#189](https://github.com/DosangGu/corvid/pull/189) · [#191](https://github.com/DosangGu/corvid/pull/191) · [#197](https://github.com/DosangGu/corvid/pull/197)
- Updates retire obsolete packages when their processes release them. A retained Claude Code terminal can restart Claude after a daemon upgrade. [#184](https://github.com/DosangGu/corvid/pull/184) · [#186](https://github.com/DosangGu/corvid/pull/186)
- Windows Desktop integrates window controls into the tab bar. Desktop anchors the host picker to its button, underlines terminal links on hover, and improves text contrast in the dark terminal theme. [#183](https://github.com/DosangGu/corvid/pull/183) · [#185](https://github.com/DosangGu/corvid/pull/185) · [#190](https://github.com/DosangGu/corvid/pull/190) · [#194](https://github.com/DosangGu/corvid/pull/194) · [#196](https://github.com/DosangGu/corvid/pull/196)
- Agent Deck shows session titles in tooltips that fit narrow windows. [#192](https://github.com/DosangGu/corvid/pull/192)

To update, open **Settings → About & updates** in Desktop or run `corvid update` for a script installation.

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.3.5/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.3.4...v0.3.5)
