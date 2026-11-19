Corvid v0.3.4 improves Desktop responsiveness and keeps Claude Code visible in Agent Deck after an in-place restart.

- Desktop renders terminal previews without holding up session updates, groups workspace notifications by frame, and moves session cleanup and storage operations off the window thread. [#173](https://github.com/DosangGu/corvid/pull/173) · [#174](https://github.com/DosangGu/corvid/pull/174) · [#175](https://github.com/DosangGu/corvid/pull/175) · [#176](https://github.com/DosangGu/corvid/pull/176) · [#177](https://github.com/DosangGu/corvid/pull/177)
- The daemon keeps directory checks outside workspace state updates and processes workspace changes in order. [#178](https://github.com/DosangGu/corvid/pull/178) · [#179](https://github.com/DosangGu/corvid/pull/179)
- Desktop terminal links end before unmatched closing brackets. [#180](https://github.com/DosangGu/corvid/pull/180)
- Agent Deck resumes tracking Claude Code when its update restarts it in the same terminal. [#181](https://github.com/DosangGu/corvid/pull/181)

To update, open **Settings → About & updates** in Desktop or run `corvid update` for a script installation.

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.3.4/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.3.3...v0.3.4)
