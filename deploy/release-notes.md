Corvid v0.5.0 brings split panes to the TUI and shares workspace navigation shortcuts with Desktop.

- The TUI shows the active tab's split layout with read-only previews of inactive panes. Click a pane or tab to focus it. [#337](https://github.com/DosangGu/corvid/pull/337)
- In TUI and Desktop, press `Ctrl+]` followed by `w` for workspaces, `t` for tabs, `p` for panes, or `?` to search commands. [#337](https://github.com/DosangGu/corvid/pull/337)
- Run `corvid list` to see workspace names and IDs, `corvid attach NAME_OR_ID` to open an existing workspace, or `corvid new NAME` to create one rooted in the current directory. [#336](https://github.com/DosangGu/corvid/pull/336)
- Long names scroll in the TUI header for the current workspace and active tab, and in the selected row of the workspace and tab pickers. [#343](https://github.com/DosangGu/corvid/pull/343)
- On Windows, the TUI preserves the final detach or shell exit status when the daemon's named pipe closes. [#341](https://github.com/DosangGu/corvid/pull/341)

[Distribution guide](https://github.com/DosangGu/corvid/blob/v0.5.0/docs/05_release_distribution.md) · [Full changelog](https://github.com/DosangGu/corvid/compare/v0.4.4...v0.5.0)
