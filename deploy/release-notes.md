Corvid v0.5.2 aligns Desktop tab and pane navigation with the TUI and adds opt-in clipboard writes to both clients.

- In Desktop tab mode (`Ctrl+]`, then `t`), use the arrows or `h`/`l` to browse the tab bar, `n` to open a tab, or `x` to close the highlighted tab. In pane mode (`Ctrl+]`, then `p`), use the arrows or `h`/`j`/`k`/`l` to browse, `r`/`d` to split, or `x` to close the highlighted pane. Enter focuses the highlighted target and Esc cancels either mode. [#354](https://github.com/DosangGu/corvid/pull/354)
- Terminal applications in the focused pane can write UTF-8 text to the clipboard using OSC 52. Enable **Allow terminal apps to copy to the clipboard** in Desktop's terminal settings. In the TUI, set `CORVID_ALLOW_CLIPBOARD_WRITE=1` and allow OSC 52 writes in a compatible outer terminal. Clipboard writes are off by default in both clients. [#351](https://github.com/DosangGu/corvid/pull/351) · [#352](https://github.com/DosangGu/corvid/pull/352)

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.5.2/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.5.1...v0.5.2)
