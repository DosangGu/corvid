Corvid v0.5.1 simplifies TUI tab and pane navigation and improves Desktop updates on macOS.

- In TUI tab mode (`Ctrl+]`, then `t`), use the arrows or `h` and `l` to highlight a tab in the top bar. Press Enter to focus it, `n` to open a tab, or `x` to close the highlighted tab immediately. [#346](https://github.com/DosangGu/corvid/pull/346)
- In TUI pane mode (`Ctrl+]`, then `p`), use the arrows or `h`, `j`, `k`, and `l` to move the highlight. Press `r` or `d` to split the highlighted pane and focus the new shell, or `x` to close the highlighted pane immediately. The hint line shows the highlighted pane number even in small terminals. [#347](https://github.com/DosangGu/corvid/pull/347)
- On macOS, restarting after a Desktop update refreshes the installed app's Launch Services registration and retries a failed launch for up to five seconds. [#348](https://github.com/DosangGu/corvid/pull/348)

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.5.1/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.5.0...v0.5.1)
