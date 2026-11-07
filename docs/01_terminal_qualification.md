# Phase 0 Terminal Qualification

The Linux spike runs one child PTY and renders its retained screen in an outer TTY.
Its results select `portable-pty` 0.9.0 and `vt100` 0.18.1 for the bounded initial
terminal profile. The production daemon and attach client will exercise the same
terminal behavior across their process boundary.

## Reproduce

```sh
cargo build --locked --workspace
cargo test --locked --workspace
python3 spikes/terminal/tests/interactive.py target/debug/corvid-terminal-spike

mkdir -p target/terminfo
tic -x -o target/terminfo spikes/terminal/corvid.terminfo
TERMINFO="$PWD/target/terminfo" cargo run --locked -p corvid-terminal-spike -- /bin/sh -i
```

The integration driver needs Linux PTYs, Python with curses, and ncurses `tic`.
The spike takes the child program and its arguments as separate CLI arguments.
Its default program is the user's shell.

## Observed Results

The checks ran on Linux 7.2.14-arch1-1 x86_64 with Rust 1.93.1 and crossterm 0.29.0.
The integration driver used an 80×24 kernel PTY with an xterm-style outer input
profile and resized it to 100×30. A separate tmux 3.8 pane check displayed Korean,
a combining mark, and RGB color.

| A19 area | Evidence |
|---|---|
| Fragmentation and input | Screen tests feed UTF-8 one byte at a time. A child behind a real PTY receives the exact split UTF-8, standalone Esc, Alt sequence, arrow sequence, and bracketed paste bytes. |
| Screen modes and colors | Screen tests check wide continuation cells, combining text, indexed and RGB colors, alternate-screen restore, cursor state, erase, and scroll regions. |
| Resize and redraw | Tests resize the screen to 300×120 and 2×2 and resize the live child PTY to 100×30. A redraw reconstructs a stale outer screen, and the dense 300×120 fixture reconstructs every cell within the frame limit. |
| Queries and terminfo | The child sees `TERM=corvid`, loads the bundled terminfo entry, and receives the expected DA, status, cursor-position, and size replies. Query bytes stay on the inner PTY path. |
| Child exit | Ctrl-C reaches the child through the kernel's line discipline. The child status is propagated and the outer TTY attributes are restored. |

`cargo tree -e features -i vte` confirmed that the std feature is disabled.
The locked VTE parser then uses a fixed 1 KiB OSC buffer. The engine caps cell text
at 22 bytes and the spike retains at most 512 history rows. The scrollback and
oversized control-string tests exercise these limits, including a 1 MiB OSC 52
payload. The retained history cell payload is bounded by 512 × 300 × 32 bytes.
Visible and alternate grids, row storage, allocator capacity, and rendering need
additional memory. Phase 1 will account for those allocations across terminals.

The lockfile licenses were inspected from Cargo metadata. The selected engine,
PTY crate, and crossterm are MIT. The transitive license expressions allow the
MIT or Apache terms with an additional Unicode-3.0 term on `unicode-ident`.
`cargo deny --locked check advisories` passed for this lockfile.

## Profile Limits

The child terminfo entry advertises the selected VT subset. Cursor reports use
absolute screen coordinates. Clipboard and window operations are ignored, and
the spike mirrors the selected legacy input modes. It forwards input in order
to its one PTY. The production client will add route fencing, a bounded paste
transaction, and modal sequence recognition. The spike writes full redraws
synchronously. Production rendering needs an independent bounded queue and must
disconnect a stalled client. Reattach, daemon survival, and resource accounting
across many PTYs are Phase 1 gates.
