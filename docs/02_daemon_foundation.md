# Linux Daemon Foundation

The daemon owns PTYs and their screens. The attach process owns the outer TTY.
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

Use `corvid attach --new-workspace "Review"` to create a separate workspace and
`corvid attach --workspace WORKSPACE_ID --new-tab "Tests"` to add a tab when that
workspace has no writer. `corvid control workspaces` lists the IDs. Attach to
an exact tab with `--workspace WORKSPACE_ID --tab TAB_ID`. In an attached TUI,
Ctrl-] then t starts a new shell tab. Ctrl-] then n or p visits the next or
previous live tab in the workspace. Each workspace has one writer, and other
workspaces continue independently.

Run `corvid attach --agent claude -- [CLI arguments]` to start the Claude Code
executable from the daemon's PATH in a dedicated PTY. The arguments after `--`
go directly to the provider. `corvid control agents` lists the resulting run and
terminal IDs with their observed process state. The Protobuf inventory includes
the exact pane location, boot ID, PID, Linux start ticks, and foreground group
when available. Corvid keeps the latest 64 retired runs in memory within the
current daemon epoch. Process probing is staggered at roughly two seconds per
managed terminal and limited to 20 procfs samples per second across the daemon.
The retained child handle supplies exit evidence, including the normal exit code.

Run `corvid hooks claude` and merge its entries into the hook arrays in your
Claude settings. The command prints an exec-form configuration for the current
Corvid executable. Start a fresh managed Claude session to load the settings.
Claude Code 2.1.318 can supply context, question, permission, and response
observations. `corvid control agents` shows collection health, pending and
unverified requests, and whether a response was observed. The installation and
coverage details are in [provider qualification](03_provider_qualification.md).

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

`corvid control clients` lists attached TUI clients, view revisions, and writer
leases. `corvid control snapshot` reads a consistent view at one daemon sequence.
`corvid control watch` prints a summary once synchronized and after each applied
transaction. Its Rust client retains the last valid cache on disconnect and
reports the watch unavailable. Establish a fresh watch to resynchronize.

In an attached TUI, press Ctrl-] then r to display a single-use pairing code.
Use Enter or Esc to restore the terminal display. Read the current host and
epoch with `corvid control host`, then run the following command and enter the
displayed code on stdin. Keep the same controller ID for subsequent commands.

```sh
corvid controller --controller-id ID --host-id HOST --server-epoch EPOCH pair
```

`corvid control clients` shows the registered client and binding revision.
`controller ... unpair --client-id CLIENT --binding-revision REVISION` revokes
that registration. `corvid control attention` lists alert IDs and request kinds.
Use `controller ... ack --run-id RUN --attention-id ALERT` to acknowledge one
alert. Its input request remains pending until the provider reports a result.

Use `corvid control agents` to find the run ID, then select it in the paired TUI.

```sh
corvid controller --controller-id ID --host-id HOST --server-epoch EPOCH \
  focus --client-id CLIENT --binding-revision REVISION --run-id RUN
```

The client must already hold the destination workspace's writer lease. Successful
focus reports the selected terminal, view revision, and daemon sequence. Bring the
local terminal window forward manually. If a paste is active, let it finish before
issuing a fresh request.

The daemon saves layout IDs and the last observed launch directory in `state.json`.
It publishes changes to a separate writer and debounces disk writes by one second.
The state directory lock allows one writer. A successful save retains the preceding
valid state in `state.json.bak`. Startup preserves bounded corrupt primary bytes in
`state.json.corrupt`, then loads a valid backup or opens an empty layout. Newer
schemas require a compatible binary. The status command reports the recovery source.
The next terminal is created by an explicit launch. Use `attach --new --restore-cwd`
to choose the recorded directory when it still exists.

## Run Under systemd

The [user unit](../deploy/systemd/corvid.service) starts Corvid under the user's
service manager and provisions the private socket directory. Install the binary,
terminal profile, and unit on the execution host.

```sh
cargo install --locked --path crates/corvid --root "$HOME/.local"
tic -x -o "$HOME/.terminfo" spikes/terminal/corvid.terminfo
install -Dm644 deploy/systemd/corvid.service "$HOME/.config/systemd/user/corvid.service"
systemctl --user daemon-reload
systemctl --user enable --now corvid.service
```

For a deployment that needs to outlive its last login, configure the user
manager's lifetime according to host policy. On hosts that allow lingering,
`loginctl enable-linger` keeps the manager running after logout. Check
`loginctl show-user "$(id -u)" -p Linger -p Sessions` when qualifying the host.
Use a test account with its own sessions and verify the daemon PID and an
advancing child counter after the final login closes and before reattaching.

## Verification

```sh
cargo test --locked --workspace
python3 crates/corvid/tests/interactive.py target/debug/corvid
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid/tests/ssh_lifecycle.py target/debug/corvid
```

The tests use real Linux Unix sockets and PTYs. The interactive driver uses an
isolated runtime, a child with a progress counter, and separate outer TTYs.

| Area | Evidence |
|---|---|
| Identity and control | Restart changes the epoch and preserves the host ID. Concurrent startup fails. Corrupt identity bytes are preserved. A real CLI subprocess relays framed control messages over pipes. |
| Snapshot and watch | A watch established before attach receives the workspace, terminal, and client at contiguous revisions. Detach removes the client while a one-shot snapshot still shows the terminal. Tests cover capture races, sequence exhaustion, slow subscribers, malformed transactions, and the stdio relay. The Rust client preserves its last view on connection loss and obtains a fresh epoch on reconnect. |
| Pairing and attention | The real TUI issues a private code, restores its redraw after dismissal, and accepts one pairing. Tests verify expiry, the host-wide guess budget, revision rotation, detach cleanup, and RPC replay. A Claude question stays pending after its exact alert is acknowledged. |
| Exact focus | Integration tests cover two independent clients, stale input tokens, paste blocking, writer ownership, and request replay. Revoking a binding after the fence restores the original view with a fresh token. |
| Layout recovery | After killing a daemon that owns a live PTY, a new daemon restores the workspace, tab, and pane IDs with an empty terminal slot. An explicit launch uses the saved directory and gets a new TerminalId. Corrupt state recovers from backup or a fresh layout, and a newer schema remains intact. |
| Writer and redraw | A competing attach at a different size is refused and leaves the child size unchanged. A successful resize resets the render generation. The detached child progresses, then the next TUI shows its retained screen and PID. |
| Fencing | An input chunk using the pre-resize token is rejected. Input with the new token reaches the child. |
| Workspaces and tabs | Two writers operate concurrently in different workspaces. Switching targets the exact existing pane and preserves the other writer. An occupied destination rejects the switch. An unowned destination requires an explicit writer claim. Old view tokens fail at the PTY boundary. |
| Managed agent process | Two isolated CLI fixtures launched in the same directory have distinct run IDs, terminal IDs, and Linux process incarnations. A verified exit changes only that run. A subsequent launch in the same pane gets new run and terminal IDs. Claude Code 2.1.318 printed its version and exited successfully through the managed launch path. |
| Paste and keys | Real TTY input covers fragmented Esc, Alt, arrows, UTF-8, literal leader, and paste in both child modes. Wire tests cover incomplete and oversized paste, busy resize, and a stalled partial write. |
| Backpressure | A client that withholds a frame acknowledgement loses its lease after five seconds. A child writing 32 KiB bursts continues and control ping stays responsive. Reattach begins with a full frame. |
| SSH lifecycle | OpenSSH 10.6p1 over loopback carries binary control over `ssh -T` and the TUI over `ssh -tt`. After clean detach and a later forced SSH disconnect, the same systemd 262 service and child PID remain active. The advancing counter and retained screen are checked on reattach. |

The current renderer shows one pane at a time with the qualified VT engine's
screen comparison. Each live PTY has a separate worker. The runtime allows up
to 32 workers and 8 attached TUI connections. Production history is limited to
128 rows per PTY to leave room for the screen and per-client comparison buffers
under the aggregate screen budget.
It sends one frame at a time and waits for the TTY write acknowledgement.
The PTY worker drains up to 128 KiB per tick and continues parsing without a writer.
The SSH fixture uses temporary keys, a pinned host key, a loopback-only SSH
listener, and a transient user unit. It ran with other user sessions active and
lingering disabled. Verification of the last logind-managed login remains a
deployment gate on a dedicated account.
