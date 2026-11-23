# Phase 0 Terminal Qualification

The Linux spike runs one child PTY and renders its retained screen in an outer TTY.
Its results select `portable-pty` 0.9.0 and `vt100` 0.18.1 for the bounded initial
terminal profile. The production daemon and attach client will exercise the same
terminal behavior across their process boundary.

## Reproduce

```sh
cargo build --locked
cargo test --locked
python3 spikes/terminal/tests/interactive.py target/debug/corvid-terminal-spike

mkdir -p target/terminfo
tic -x -o target/terminfo crates/corvid-daemon/assets/corvid.terminfo
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

## libghostty-vt evaluation

Issue [#377](https://github.com/DosangGu/corvid/issues/377) tracks migration of
the daemon terminal engine. The optional daemon feature `ghostty-vt` links
libghostty-vt 1.0.1. Use Zig 0.17.1 on `PATH` for targets that build the library
from source.

```sh
cargo test --locked -p corvid-daemon --features ghostty-vt --lib \
  terminal::screen::ghostty_qualification
cargo build --locked -p corvid-daemon --features ghostty-vt
CORVID_VT_ENGINE=ghostty target/debug/corvidd daemon
```

New sessions in an opt-in daemon use Ghostty for PTY output, reflow, cursor and
input modes, query replies, and history. Retained sessions keep the engine of
their existing terminal host. Native terminal and render handles stay on their
creating worker thread. Corvid projects the supported text cells and styles into
the existing ANSI encoder and uses each client's acknowledged frame as its
comparison baseline. Corvid handles clipboard consent and delivery.

The release build still selects `vt100`. The projection supports the existing
text rendition and input profile. Extended formatting, Kitty graphics and
keyboard negotiation, and resource profiling remain follow-up gates.

The `Checks / libghostty-vt` workflow builds the opt-in daemon and TUI for the
five release targets. It runs the linked VT fixtures on each target, PTY
routing and handoff on Linux, the terminal relay on Linux and macOS, and a
ConPTY shell history test on Windows.

### Comparing engine costs

Run `python3 spikes/terminal/benchmark_engines.py` with Zig on `PATH`. The
driver builds a release-mode daemon test binary and emits JSON Lines for each
engine, synthetic Codex and Claude Code workload, and session count. Each case
runs in a fresh process. Use `--sessions 1 32 64` to extend the idle fleet.

`parse_ms` covers fragmented PTY output and reply handling. The frame timings
include encoding and retaining the next acknowledged snapshot. Each frame
starts from a fresh screen mutation. Divide by `frames_per_kind` to get mean
cost and bytes per full or incremental frame. `exported_history_bytes` is the
current bounded history wire size. Linux reports the idle process RSS delta
after all sessions retain scrollback. Unix CPU time covers the complete child
process and is rounded at the OS clock resolution. These synthetic cases do
not include PTY scheduling, clients, or external agent workloads.

### Idle scrollback on Linux

The libghostty worker compresses cold scrollback after two seconds without
terminal output, resize, or a history read. While input and output are idle,
it allows at most one incremental step every 250 ms. It resumes after the
library reports new compressible activity. The same worker owns parsing,
rendering, history, and compression. Unsupported kernels and compression
errors leave the normal bounded scrollback in use for that terminal.

### Kitty graphics qualification and proposed client transport

The linked fixture transmits raw RGBA pixels, inspects the placement geometry,
and observes image replacement, screen changes, and deletion. Retransmitting an
image changes its generation and removes its existing placements. Returning
from the alternate screen restores the primary screen's placements. Consumers
must copy needed pixel data while the engine owns the snapshot and discard
cached textures when the image generation or active route changes.

The production text profile disables image storage. A proposed graphics
capability would carry separately bounded image data and placement changes
alongside ANSI frames. Image application would share the existing route,
render generation, and `FrameApplied` boundary. Reattach and route changes
would resend the visible image set. The first profile would accept direct
pixel transfers only, with file, temporary-file, and shared-memory media
disabled at the engine. Bound storage per terminal and bound both the pixel
count and bytes before decoding or forwarding.

GPUI would layer textures over the text grid using the engine-provided source
rectangle, cell placement, and draw layer. TUI would advertise the capability
only after a successful probe of its outer terminal. It would emit newly
encoded Kitty direct transfers with route-local IDs and delete only its own
placements. Clients without the capability keep the existing ANSI text
profile. These client paths and their wire format are proposals.
