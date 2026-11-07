# Linux Daemon Foundation

The daemon owns the PTY and the screen. The attach process owns the outer TTY.
The control socket supports host identity, health, workspace inventory, and ping
independently of the attached TUI.

## Run Locally

```sh
cargo build --locked
mkdir -p target/terminfo
tic -x -o target/terminfo spikes/terminal/corvid.terminfo
TERMINFO="$PWD/target/terminfo" target/debug/corvid daemon
```

In a second terminal, run `target/debug/corvid attach --new` to start a shell,
or provide an explicit command with `target/debug/corvid attach -- program arg`.
Use `corvid attach` to join the retained terminal. Press Ctrl-] then d to detach.
Ctrl-] twice sends one literal Ctrl-]. The daemon returns an unmatched leader
to the PTY after two seconds. A signal or connection loss also releases the writer.
The shell's exit status reaches the attach process on normal completion.

The TUI recognizes split legacy VT keys, UTF-8, and bracketed paste. The
`--escape-delay-ms` option defaults to 100 ms. Network delays above this setting
can split an Alt key into literal Esc and a subsequent key. The daemon receives
paste as a route-scoped transaction with a 1 MiB limit. The outer TTY always
captures paste and the daemon adds delimiters when the child enables that mode.
An accepted write holds the input barrier until it finishes or stalls.

Run `target/debug/corvid control host` to read the stable HostId and current
ServerEpoch. Run `corvid control workspaces` for workspace, tab, and pane IDs and
`corvid control status` for live terminal and save status. The binary stdio relay
runs as `corvid control --stdio` over a
non-PTY connection. Give the runtime directory a trusted absolute path through
`XDG_RUNTIME_DIR`, `CORVID_RUNTIME_DIR`, or `--runtime-dir`.

The daemon saves layout IDs and the last observed launch directory in `state.json`.
It publishes changes to a separate writer and debounces disk writes by one second.
The state directory lock allows one writer. A successful save retains the preceding
valid state in `state.json.bak`. Startup preserves bounded corrupt primary bytes in
`state.json.corrupt`, then loads a valid backup or opens an empty layout. Newer
schemas require a compatible binary. The status command reports the recovery source.
The next terminal is created by an explicit launch. Use `attach --new --restore-cwd`
to choose the recorded directory when it still exists.

## Verification

```sh
cargo test --locked --workspace
python3 crates/corvid/tests/interactive.py target/debug/corvid
```

The tests use real Linux Unix sockets and PTYs. The interactive driver uses an
isolated runtime, a child with a progress counter, and separate outer TTYs.

| Area | Evidence |
|---|---|
| Identity and control | Restart changes the epoch and preserves the host ID. Concurrent startup fails. Corrupt identity bytes are preserved. A real CLI subprocess relays framed control messages over pipes. |
| Layout recovery | After killing a daemon that owns a live PTY, a new daemon restores the workspace, tab, and pane IDs with an empty terminal slot. An explicit launch uses the saved directory and gets a new TerminalId. Corrupt state recovers from backup or a fresh layout, and a newer schema remains intact. |
| Writer and redraw | A competing attach at a different size is refused and leaves the child size unchanged. A successful resize resets the render generation. The detached child progresses, then the next TUI shows its retained screen and PID. |
| Fencing | An input chunk using the pre-resize token is rejected. Input with the new token reaches the child. |
| Paste and keys | Real TTY input covers fragmented Esc, Alt, arrows, UTF-8, literal leader, and paste in both child modes. Wire tests cover incomplete and oversized paste, busy resize, and a stalled partial write. |
| Backpressure | A client that withholds a frame acknowledgement loses its lease after five seconds. A child writing 32 KiB bursts continues and control ping stays responsive. Reattach begins with a full frame. |

The single-pane renderer uses the qualified VT engine's screen comparison.
It sends one frame at a time and waits for the TTY write acknowledgement.
The PTY worker drains up to 128 KiB per tick and continues parsing without a writer.
This fixture covers local detach and reconnect. The deployment gate includes
working SSH loss and last logout under a configured service.
