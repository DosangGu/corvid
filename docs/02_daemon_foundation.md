# Daemon Foundation

The daemon service owns PTYs and their screens. Automatically started daemons
delegate each terminal to a private terminal-host process. The attach process owns the outer TTY.
The control socket supports host identity, health, workspace inventory, and ping
independently of the attached TUI.

## Run Locally

```sh
cargo build --locked -p corvid-tui
target/debug/corvid
```

Linux builds use the ncurses `tic` compiler. Cargo builds the Protobuf schema
using `protox`. Running `corvid`
without a subcommand starts a background daemon if needed and joins a live
terminal in the default workspace. It opens the user's shell when the workspace
has no live terminal. To start a shell explicitly, use `corvid attach --new`.
The automatically started daemon uses the bundled terminfo entry.
Use `corvid ensure-daemon` to start or reuse the same background daemon
without attaching a TTY. It shares the startup lock with the ordinary CLI.
For a daemon that must survive the host's last-login policy, follow the
[systemd setup](#run-under-systemd).

After building or installing an updated binary, run `target/debug/corvid upgrade`
or `corvid upgrade` with the updated executable. Linux and Windows use the same
planned handoff for automatically started daemons. The daemon fences terminal input,
detaches current clients, waits for metadata persistence, and records the terminal-host
identities in a private `handoff.json` in the runtime directory.
The replacement keeps the same state directory, host identity, layout, terminal IDs,
PTY processes, and terminal screen state, and starts a new daemon epoch. Reattach a TUI
after the connection closes. Desktop retries a lost connection against the same host and
terminal ID, fences queued actions, and acquires a fresh writer lease.
An active paste or unfinished view operation must finish before handoff.
If the runtime is stopped, the command starts the updated daemon.
For foreground or systemd daemons, use their existing start and stop procedure.
An older daemon without terminal-host support needs one initial idle upgrade or
manual restart before its new terminals can survive subsequent replacements.

The shared terminal backend owns input queues, VT query replies, parsing, resize,
preview, and rendering. Its versioned, bounded Protobuf protocol uses private Unix
sockets on Linux and per-user named pipes on Windows. Terminal content stays in the
terminal host's memory. A retained host continues draining output and answering VT
queries while the replacement starts. The handoff file contains terminal identities
and layout locations. Windows uses a separate executable image for terminal hosts
so they can remain alive while the daemon executable is replaced.

Client identities, writer leases, and input routes are freshly established after
handoff. Queued user input is fenced. Agent observations from the previous epoch
remain historical. New agent invocations from a retained Linux shell register against
the current daemon epoch. Existing provider processes retain their original observer
environment and may need a fresh invocation to restore qualified hook monitoring.

Press Ctrl-] then d to detach and run `corvid` again to return to the shell.
The TUI remembers its last successful daemon endpoint and workspace/tab/pane selection
separately from Desktop. Plain `corvid` or `corvid attach` restores that view after
verifying the host identity and reading a fresh layout snapshot. `--runtime-dir` and
`CORVID_RUNTIME_DIR` override the saved endpoint; explicit workspace/tab/pane selectors,
new-terminal commands, and layout commands override the saved selection. A TUI launched
through SSH remembers the daemon on that execution host; it does not launch SSH or WSL.

If a saved pane or tab was removed, restoration chooses the first remaining pane in
that tab or the first tab in the same workspace. If the workspace no longer exists,
choose a target explicitly with `corvid attach --workspace ID --tab ID --pane ID`.
If the saved terminal is no longer running, use an explicit launch such as
`corvid attach --new --workspace ID --tab ID --pane ID`. Restoration never starts a
replacement process; first use without a saved view retains the default-shell behavior.

TUI preferences use private JSON at `%LOCALAPPDATA%\Corvid\clients\tui\view.json`
on Windows and `$XDG_STATE_HOME/corvid/clients/tui/view.json` (or
`~/.local/state/corvid/clients/tui/view.json`) on Linux. `CORVID_VIEW_STATE_DIR`
overrides the parent of the `tui` directory, including in isolated test fixtures.
Changes are coalesced for 250 ms by a separate writer and flushed on clean exit.
An atomic replacement retains `view.json.bak`; malformed state falls back to the
backup or default startup, while unsupported schemas and oversized files are preserved.
Each target has one last saved selection, so concurrent TUIs use the most recently
saved selection for their next startup. Failed connections do not overwrite it.

The bottom status bar shows **CORVID**, the current workspace, and tab numbers
and names, such as **[1:Shell]** and **2:Tests**. Brackets and highlighting
identify the active tab. Press Ctrl-] to show command mode and available keys.
The bar reserves one row on terminals with at least three rows when both
peers support it.

Use `corvid attach -- program arg` to start an explicit command on the running
daemon. When starting a daemon directly, install the terminfo entry with
`tic -x -o "$HOME/.terminfo" spikes/terminal/corvid.terminfo` first.

New Linux PTYs use `TERM=xterm-corvid-256color`. The name enables title output
in shells that recognize the `xterm*` prefix, while the bundled entry advertises
Corvid's own VT profile. The `corvid` terminfo name remains available for older
PTYs. To pick up the new `TERM` after a daemon upgrade, open a new terminal.

Ctrl-] enters command mode for the next complete key. Ctrl-] twice sends one
literal Ctrl-]. An unmapped key sends the leader and the key as terminal input.
The daemon returns an idle leader to the current PTY after two seconds.
Esc or Ctrl-C cancels a pending leader without sending it to the terminal.
Ctrl-] then `?` opens a persistent command sheet. It shows only negotiated
operations and distinguishes detaching from ending terminals. Unmapped keys and
bracketed paste stay in this sheet; Esc or Ctrl-C closes it, a command key runs
that command, and Ctrl-] sends one literal leader and closes it. The sheet and
the brief prefix hints paint over the existing screen without resizing the PTY.
A signal or connection loss also releases the writer.
The shell's exit status reaches the attach process on normal completion.

Use `corvid attach --new-workspace "Review"` to create a separate workspace and
`corvid attach --workspace WORKSPACE_ID --new-tab "Tests"` to add a tab when that
workspace has no writer. `corvid control workspaces` lists the IDs. Attach to
an exact tab with `--workspace WORKSPACE_ID --tab TAB_ID`. In an attached TUI,
Ctrl-] then t starts a new shell tab. Ctrl-] then 1–9 switches to the live tab
with that number. Ctrl-] then w opens the full tab list for the workspace,
including tabs numbered 10 and above. Type to filter tab names, Backspace to edit
the query, Up or Down to move, Enter to switch to a live tab, and Esc to return
to the current tab. Closed tabs are labeled in the list alongside their pane
counts. Ctrl-] then n or p still cycles through live tabs. Pane writers resize
their selected pane; hidden panes retain their size until selected. Other
writers keep their own viewports.

Press Ctrl-] then `%` to split right, `"` to split down, or `o` to cycle
through live panes in the tab. Use `corvid attach --workspace WORKSPACE_ID
--tab TAB_ID --pane PANE_ID` to reattach to an exact pane. Windows Desktop
creates and selects these same daemon-owned workspaces, tabs, and panes.

To move a hidden live single-pane tab, attach to a different tab in its
workspace and explicitly claim the destination for that TUI.

```sh
corvid attach --workspace "$SOURCE" --tab "$VISIBLE_TAB" \
  --claim-workspace "$DESTINATION" --move-tab "$HIDDEN_TAB" \
  --to-workspace "$DESTINATION"
```

The move retains the pane, terminal, and run IDs and keeps the visible tab
selected. It publishes the layout and new location together. A companion slot
that already points to the run can focus it in the claimed workspace.

Run `corvid attach --agent claude -- [CLI arguments]` to start the Claude Code
executable from the daemon's PATH in a dedicated PTY. The arguments after `--`
go directly to the provider. `corvid control agents` lists the resulting run and
terminal IDs with their observed process state. The Protobuf inventory includes
the exact pane location, boot ID, PID, Linux start ticks, and foreground group
when available. Corvid keeps the latest 64 retired runs in memory within the
current daemon epoch. Process probing is staggered at roughly two seconds per
managed terminal and limited to 20 procfs samples per second across the daemon.
The retained child handle supplies exit evidence, including the normal exit code.

A managed launch detects the provider release and adds observation hooks to
the new session when the release is qualified. Codex asks you to review and
trust new hooks through its own UI. `corvid control agents` shows collection
health, pending and unverified requests, and whether a response was observed.
Claude Code and Codex can run in separate tabs of the same workspace. Detach
between the following commands, then use Ctrl-] followed by n or p to switch tabs.

```sh
corvid attach --new-tab Claude --agent claude
corvid attach --new-tab Codex --agent codex
```

See [provider qualification](03_provider_qualification.md) for coverage and
manual hook configuration.

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

Desktop uses `corvid terminal-relay` over non-PTY SSH for the interactive
protocol. That relay reaches `client.sock` and forwards bounded structured
messages and ANSI render frames. Sibling pane previews are rate limited and
carry no input route. The control relay still carries structured control
messages through `control.sock`.

Run `corvid control capabilities` to list the negotiated control features,
command limits, writer policy, and the daemon build's exact provider profiles.
The profiles include adapter schema revisions, event and correlation fields,
and qualification levels with their reasons. A control or TUI Hello reports
the same provider profiles. A managed launch checks the provider executable,
and `corvid control agents` reports the resulting adapter and collection state
for that run.

Run `corvid control collector-status` to count current top-level managed runs
by their observed collection state. The counters for accepted and rejected
submissions, failed connections, and saturated admission reset each daemon
epoch. Late connections count toward the epoch that receives them. A connection
can fail while sending a reply after the collector has
already accepted its submission. An unattributed transport failure affects
the traffic counts only. Per-run gap evidence comes from the verified hook
or process association.

The Unix socket checks both filesystem ownership and peer UID. A rejected
peer is dropped before the handshake. Frame assembly has a five-second
deadline from its first byte. An idle attached TUI can wait between frames.

`corvid control clients` lists attached TUI clients, view revisions, writer
leases, and input mode. Each connection's keymap enables tab and pairing
commands according to its attach capabilities. A new view returns the affected
client to passthrough and keeps its negotiated keymap. Other clients retain
their own modes and keymaps. `corvid control snapshot` reads a consistent view
at one daemon sequence.
`corvid control watch` prints a summary once synchronized and after each applied
transaction. Its Rust client retains the last valid cache on disconnect and
reports the watch unavailable. Establish a fresh watch to resynchronize.

The daemon admits up to eight snapshot watches across sixteen control
connections. Each watch queues up to 1,024 deltas or 4 MiB of changes. The
daemon reports `ResyncRequired` if it detects overflow at a frame boundary.
If overflow interrupts a frame write, it closes that connection. A fresh
watch always starts with a full snapshot.

In an attached TUI, press Ctrl-] then r to display a single-use pairing code.
Use Enter or Esc to restore the terminal display. Read the current host and
epoch with `corvid control host`, then run the following command and enter the
displayed code on stdin. Keep the same controller ID for subsequent commands.

```sh
corvid controller --controller-id ID --host-id HOST --server-epoch EPOCH pair
```

`corvid control clients` shows the registered client and binding revision.
`controller ... unpair --client-id CLIENT --binding-revision REVISION` revokes
that registration. `corvid control attention` lists alert IDs and the IDs,
kinds, and statuses of associated requests.
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

The daemon saves layout IDs, the last observed working directory, and allowlisted
run history in `state.json`. It publishes changes to a separate writer and
debounces disk writes by one second. The archive retains up to 128 runs and
keeps current top-level running agents ahead of older records when evicting.
The state directory lock allows one writer. A successful save retains the preceding
valid state in `state.json.bak`. Startup upgrades validated schema 1, 2, and 3 files
to schema 4 and keeps the original as the backup. Schema 1 starts with an empty
archive. Schema 3 adds a process-directory observation source. Schema 4 stores
optional manual tab names and accepts older files containing pane name overrides.
The daemon ignores those overrides. Legacy `Shell` and `Shell N` tab labels become
automatic; other labels remain manual. Older schemas did not distinguish an
explicitly chosen `Shell` label from the generated default. Startup
preserves bounded corrupt primary bytes in
`state.json.corrupt`, then loads a valid backup or opens an empty layout. Newer
schemas require a compatible binary. The status command reports the recovery source.

Run `corvid control history` after a daemon restart to inspect saved records
under their original epochs. History includes the last observed state, request
kinds and statuses, and attention acknowledgments. The current process state of
a historical run is Unknown, and an unresolved request is Unverified. Snapshot
clients request this separate archive with `include_history`. It shares the
snapshot transfer limit and stays outside the live entity and focus graph.

The next terminal is created by an explicit launch. Use `attach --new --restore-cwd`
to choose the recorded directory when it still exists. The daemon records the
launch directory and samples `/proc/<pid>/cwd` for the original PTY child, even
when its tab is detached. It checks the process start time and verifies that the
observed path names the same directory in the daemon's namespace. A valid change
is stored with its observation time on the pane's current tab. If a sample is
unavailable, Corvid keeps the last valid directory.
[Linux `/proc/<pid>/cwd` reference](https://man7.org/linux/man-pages/man5/proc_pid_cwd.5.html)

## Response Notifications over SSH

Run `corvid` in an interactive SSH session, or start it with
`ssh -tt work-host corvid`. When Corvid detects an SSH TTY, it uses that
connection to send a terminal bell for the first verified response to the
current prompt of a managed Claude or Codex run. The terminal controls how
the bell is presented. For WezTerm, see its
[bell settings](https://wezterm.org/config/lua/config/audible_bell.html).

Inside a newly opened Corvid shell, run `claude` or `codex` normally. Corvid
resolves the executable on that shell's PATH, verifies that the foreground
process belongs to the Corvid PTY, and enables session-scoped observers for
qualified releases. Codex may ask you to trust the hooks in its own UI.
This works with `ssh -tt work-host corvid` on the updated Linux host. Use a new
shell after updating the daemon so the shell receives the command wrappers.

Bash and Zsh keep the two command names connected when a login profile
prepends another directory to PATH. Other shells use the private PATH
directory directly. An absolute path or an alias that runs the executable
directly bypasses the wrappers. The explicit `corvid attach --agent claude`
and `corvid attach --agent codex` commands also load the observers for
qualified releases.

A provider `Stop` observation means a response was observed. Other hooks or
background work can continue afterward. Corvid sends the signal to the live
TUI holding the workspace writer lease, including for agents in background
tabs. Only events observed while attached are eligible for delivery.

Set `CORVID_NOTIFY=osc9` on the remote `corvid` command to request a toast
through a terminal that supports OSC 9. The desktop and terminal notification
settings determine whether it is shown. Use `CORVID_NOTIFY=off` to disable
the signal or `CORVID_NOTIFY=bell` to enable it outside SSH. Both the Linux
daemon and the attach client must support the response notification capability.

## Run Under systemd

The [user unit](../deploy/systemd/corvid.service) starts Corvid under the user's
service manager and provisions the private socket directory. Install the binary,
terminal profile, and unit on the execution host.

```sh
cargo install --locked --path crates/corvid-tui --root "$HOME/.local"
tic -x -o "$HOME/.terminfo" spikes/terminal/corvid.terminfo
install -Dm644 deploy/systemd/corvid.service "$HOME/.config/systemd/user/corvid.service"
systemctl --user daemon-reload
systemctl --user enable --now corvid.service
```

For a deployment that needs to outlive its last login, configure the user
manager's lifetime according to host policy. On hosts that allow lingering,
`loginctl enable-linger` keeps the manager running after logout. Check
`loginctl show-user "$(id -u)" -p Linger -p Sessions` when qualifying the host.

For A21, use a disposable systemd host with OpenSSH configured to register
sessions through PAM and `pam_systemd`. Set up the unit, Corvid, Python 3, and the
terminfo entry for a dedicated `operator` account and enable lingering for that
account with operator authorization. Configure a separate `observer` account
with noninteractive sudo and pin the host key for both SSH aliases. Run from the
repository root on the client machine.

```sh
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid-tui/tests/last_login.py ssh_config operator observer
```

The observer checks that each disconnect closes the only SSH login and that the
user manager remains lingering. It compares the daemon epoch, daemon and child
process incarnations, and PTY device while the child counter advances. The next
attach must show the same child and retained screen.

## Run Locally on Windows

Build the Windows x64 TUI and daemon together with the pinned MSVC Rust
toolchain. Open a VT-capable console such as Windows Terminal.

```powershell
cargo build --locked -p corvid-tui
.\target\debug\corvid.exe
```

`corvid` starts a detached per-user daemon as needed and attaches to its
default workspace. The daemon owns the PowerShell ConPTY and applies the same
writer lease, layout, and render protocol as on Linux. Press Ctrl-] then d to
detach, and run `corvid` again to reattach. Ctrl-] then t opens a tab, and
Ctrl-] then % or " splits the current tab.

The local client and control pipes are limited to the current Windows user
and reject remote clients. Runtime coordination files are under
`%LOCALAPPDATA%\Corvid\runtime`. The daemon persists its host identity and
workspace metadata in `%LOCALAPPDATA%\Corvid\daemon`. Use
`corvid control ping` to check the daemon and `corvid control host` to inspect
its identity. For an isolated foreground instance, use
`corvid --runtime-dir PATH daemon --state-dir PATH` with separate paths on a
local drive with ACL support. Corvid creates the private directories if
needed. Pass the same runtime directory to the TUI. Ctrl+C stops that
foreground daemon.

Windows support covers local terminal sessions and Windows x64 OpenSSH
connections through Desktop. The Desktop bundle can install and start the
Windows daemon for the SSH user. See [Desktop SSH setup](04_companion.md#windows-quick-start).
Managed agent observations on Windows are planned separately.

The `windows_daemon` integration tests run the Windows binary inside ConPTY
and exercise startup, input, rendering, resize, and detach and reattach.
Windows console input preserves VT sequences and bracketed-paste boundaries
before routing input to the daemon. The command-menu regression verifies that
unmapped keys and pasted shell commands stay out of the PTY, the menu remains
open past the prefix timeout, and closing it preserves terminal identity and
dimensions. UTF-16 input tests cover surrogate pairs and control characters.

```powershell
cargo test --locked -p corvid-tui --test windows_daemon
```

## Verification

```sh
cargo test --locked --workspace
python3 crates/corvid-tui/tests/interactive.py target/debug/corvid
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid-tui/tests/ssh_lifecycle.py target/debug/corvid
```

The tests use real Linux Unix sockets and PTYs. The interactive driver uses an
isolated runtime, a child with a progress counter, and separate outer TTYs.

| Area | Evidence |
|---|---|
| Default startup | Two simultaneous invocations share one background daemon. The first writer opens a shell with the bundled terminfo entry, and the other receives `WorkspaceBusy`. After detach, another invocation reaches the same shell PID. A non-TTY invocation and a control ping leave an absent runtime untouched. |
| Status bar | The live TTY test checks the bottom row, active tab, idle command mode, and the reduced child size across resize and tab switching. The VT composition test verifies cursor position, pending wrap, drawing attributes, and scrolling above the bar. |
| Identity and control | Restart changes the epoch and preserves the host ID. Concurrent startup fails. Corrupt identity bytes are preserved. A real CLI subprocess relays framed control messages over pipes. |
| Version skew (A15) | A client offering protocol 1.1 uses 1.0 with the daemon from `3d804e1`. The daemon rejects a 2.0 client and reports its supported major. The same daemon epoch, client view, and child PID survive the rejection. An incompatible upstream framing preamble becomes `ProtocolMismatch` on both the local CLI and stdio relay. |
| Capability discovery (A29) | `GetCapabilities` returns the negotiated limits, features, and release profiles from the control Hello over binary stdio. The local CLI reports both provider profiles while the provider launch probes remain untouched. Duplicate profile identities invalidate Hello, and an additive provider ID remains descriptive data. |
| Live state compatibility (A29) | An unknown agent lifecycle in a delta returns `ResyncRequired`, rolls back the whole transaction, and preserves the previous view and sequence. An unknown request status during catch-up prevents the watch from becoming ready. Explicit Unknown states remain valid. |
| Collector status (A17, A22) | A closed collector handshake and an unowned hook submission increment separate counters. Restart clears both. Current coverage counts exclude exited and archived runs. Claude fault fixtures distinguish a verified per-run gap from saturation at the 32-connection admission limit. |
| Socket boundary (A24, A30) | A two-UID Linux fixture checks filesystem access and both peer credential checks. Real TUI and control sockets reject invalid lengths, kinds, and Protobuf while a separate PTY handles input. A partial request held open after `Hello` reaches the assembly deadline and its connection closes. |
| Snapshot and watch | A watch established before attach receives the workspace, terminal, and client at contiguous revisions. Detach removes the client while a one-shot snapshot still shows the terminal. Tests cover capture races, sequence exhaustion, slow subscribers, malformed transactions, and the stdio relay. The Rust client preserves its last view on connection loss and obtains a fresh epoch on reconnect. |
| Pairing and attention | The real TUI issues a private code, restores its redraw after dismissal, and accepts one pairing. Tests verify expiry, the host-wide guess budget, revision rotation, detach cleanup, and RPC replay. A Claude question stays pending after its exact alert is acknowledged. |
| Exact focus (A3) | Two clients use different keymaps and enter command mode. Focusing a new run clears the focused client's prefix and retains its keymap. The other client keeps its complete view and completes its original pairing command. Tests also cover stale input tokens, paste blocking, writer ownership, and request replay. Revoking a binding after the fence restores the original view with a fresh token. |
| Remote reboot (A11) | An exact focus request succeeds through the SSH stdio relay before a Debian 13 QEMU guest reboots. The kernel boot ID and daemon epoch change. The host ID and layout persist, live inventory starts empty at sequence zero, and the old run appears as Historical with Unknown process state. The new daemon rejects the identical Protobuf request with `EpochMismatch` and leaves the snapshot unchanged. |
| Layout recovery (A23) | A shell changes directory and emits a forged OSC 7 path. After the daemon is killed, the new daemon restores the layout with an empty terminal slot. An explicit launch uses the observed shell directory and gets a new TerminalId. Schemas 1 and 2 migrate with a backup. Corrupt state recovers from backup or a fresh layout, and a newer schema remains intact. |
| Agent history after restart (A23) | With a Claude Code 2.1.318 question open, the fixture acknowledges its alert, waits for persistence, and kills the daemon. The same host starts a new epoch with an empty live inventory. The old question and acknowledgment are visible in history, the question is Unverified, the old focus returns `EpochMismatch`, and the companion slot is Historical. |
| Writer and redraw | A competing attach at a different size is refused and leaves the child size unchanged. A successful resize resets the render generation. The detached child progresses, then the next TUI shows its retained screen and PID. |
| Fencing | An input chunk using the pre-resize token is rejected. Input with the new token reaches the child. |
| Workspaces and tabs | Two writers operate concurrently in different workspaces. Switching targets the exact existing pane and preserves the other writer. An occupied destination rejects the switch. An unowned destination requires an explicit writer claim. Old view tokens fail at the PTY boundary. |
| Moving a live tab (A2) | The real TUI moves a hidden tab into a claimed workspace and reattaches to the same child PID. The wire test rejects occupied or unclaimed destinations and the visible tab. An active watch applies the new layout, terminal location, and agent location together. A companion capture made before the move focuses the same run and redraws its retained screen. |
| Snapshot catch-up race (A8) | A real Claude question and tab move commit after `SnapshotBegin`. The base chunks keep the old location and exclude the new request. Contiguous deltas carry the request and alert, then both workspaces and the new agent and terminal locations together. The Rust watch first publishes at `WatchReady` after both changes. |
| Managed agent process | Two isolated CLI fixtures launched in the same directory have distinct run IDs, terminal IDs, and Linux process incarnations. A verified exit changes only that run. A subsequent launch in the same pane gets new run and terminal IDs. Claude Code 2.1.318 printed its version and exited successfully through the managed launch path. |
| PID and pane reuse (A12) | Two managed runs in the same pane and companion slot reuse PID 4096 and the same executable inode with different start times. With a fresh pairing, the old run returns `TargetGone` and the old slot capture returns `StaleBinding`. The client view and layout stay the same. The replacement capture focuses the new terminal without injecting input. |
| Silent process changes (A27) | A managed shell fixture changes its foreground group without terminal output, then replaces its executable under the same PID. Focus rejects the old run even before the periodic sample, and the next sample retires that run while its PTY stays live. |
| Paste and keys | Real TTY input covers fragmented Esc, Alt, arrows, UTF-8, literal leader, and paste in both child modes. Wire tests check the exact bytes for an expired leader and an unmapped suffix, incomplete and oversized paste, busy resize, and a stalled partial write. |
| Backpressure (A13) | Eight watchers stop reading while the daemon publishes 48 moves between large workspaces. A ninth watch receives `LimitExceeded`, control ping and input to a separate PTY succeed, and a new watch resynchronizes after overflow. The output-heavy PTY answers cursor position queries while the stalled renderer loses its lease. Reattach begins with a full frame. |
| Untrusted terminal output (A18) | A managed Claude fixture writes forged OSC and question text into a live PTY. The outer TTY displays literal text, and the exact request stays pending until the fixture answers through the terminal. |
| SSH lifecycle | OpenSSH 10.6p1 over loopback carries binary control over `ssh -T` and the TUI over `ssh -tt`. After clean detach and a later forced SSH disconnect, the same systemd 262 service and child PID remain active. The advancing counter and retained screen are checked on reattach. |
| Last login (A21) | Debian 13 in a disposable QEMU guest ran the shipped user unit with systemd 257.13 and OpenSSH 10.0p2 using PAM. The fixture passed after clean detach and forced SSH loss with `KillUserProcesses=yes`, `UserStopDelaySec=0`, and lingering enabled for the Corvid account. |
| Schema generation (A31) | Two clean Linux builds produce identical Prost types and Rust wire preflight tables. The CLI and framing tests run with the compiler unavailable, including round trips of exact `uint64` values and opaque bytes. |

The current renderer shows one pane at a time with the qualified VT engine's
screen comparison. Each live PTY has a separate worker. The runtime allows up
to 32 workers and 8 attached TUI connections. Production history is limited to
128 rows per PTY to leave room for the screen and per-client comparison buffers
under the aggregate screen budget.
It sends one frame at a time and waits for the TTY write acknowledgement.
The PTY worker drains up to 128 KiB per tick and continues parsing without a writer.
The VT engine captures OSC 0/2 terminal titles (BEL or ST terminated) as bounded
display data, including titles emitted while detached. It consumes clipboard
commands. Titles never become terminal input, shell code, or agent identity.
Both full and
incremental redraws strip Unicode `Bidi_Control` characters from cell output
and preserve visible Unicode and cell positions.
[Unicode character properties](https://www.unicode.org/Public/17.0.0/ucd/PropList.txt)

Pane titles follow the latest title from their respective terminals. An
automatic tab with one pane follows that pane's title. An automatic split tab
uses `Tab N`, where N is its position in the workspace. A manual tab name takes
precedence and survives splits. Until a terminal supplies a nonempty title, a pane
uses `Pane N`. Printable Unicode, spaces, and semicolons are preserved; control
and bidi-control characters are removed, and titles are bounded to 1,024 UTF-8
bytes. Terminal titles are transient; only manual names are written to state.
Retained terminal hosts report their latest title again after daemon replacement.

Press Ctrl-] then `.` to rename the current tab. Enter saves, Escape cancels,
Ctrl-U clears, and saving an empty name restores automatic naming. The editor
consumes input and bracketed paste without sending it to the child process.
Split tabs show the active pane title beside the tab strip. The tab picker
updates names while open.
Clients negotiate `view_titles.v1`; `RenameView` requires the current view
revision and writer ownership. A rename preserves the input route and PTY.

PTY process checks run on a staggered two-second timer. PTY output can bring a
check forward after a 500 ms debounce. Due checks wait in arrival order, at least
50 ms apart host-wide. At the 32-PTY limit, allow about four seconds for a silent
change when worker ticks run on schedule, plus host scheduling delays. Managed
runs also track the foreground group and executable device and inode. Foreground
changes provide process evidence only. An exec of the same executable file can
leave this evidence unchanged. Focus checks the live process before applying a
selection.
[Linux `/proc` reference](https://docs.kernel.org/filesystems/proc.html#process-specific-subdirectories)

The SSH fixture uses temporary keys, a pinned host key, a loopback-only SSH
listener, and a transient user unit. It ran with other user sessions active and
lingering disabled. Use the last-login fixture to qualify the logout policy on
the intended deployment host.

Reproduce the A23 history check with Claude Code 2.1.318 and the loopback
provider fixture. It also checks that the synthetic prompt and question are
absent from the stored metadata.

```sh
python3 crates/corvid-tui/tests/claude_qualification.py --corvid target/debug/corvid --scenario restart
```

### Remote reboot reproduction

A11 ran on QEMU 11.1.3 with Debian 13, Linux 6.12.111, systemd 257.13, and
OpenSSH 10.0p2. Use the dedicated `operator` and `observer` accounts and
pinned SSH setup described for A21. The user unit must start at boot, and the
daemon's live inventory must be empty when the fixture starts. In the
disposable guest, install this test executable as
`/usr/local/bin/claude` on the daemon's PATH.

```python
#!/usr/bin/python3
import os
import sys
import tty

if sys.argv[1:] != ["--reboot-probe"]:
    raise SystemExit(1)

tty.setraw(0)
os.write(1, b"REBOOT-READY")
assert os.read(0, 4096) == b"q"
```

The version probe selects the process-only adapter. The fixture checks that
both SSH aliases refer to separate accounts in the same QEMU guest and uses
the observer account to reboot it. The saved focus request travels over a
fresh relay connection after reboot, so the rejection comes from the daemon.

```sh
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid-tui/tests/remote_reboot.py \
  ssh_config operator observer --reboot-disposable-vm
```

### Snapshot catch-up race reproduction

The A8 fixture uses Claude Code 2.1.318 and the loopback model server.
It seeds 80 synthetic historical runs with 256 requests each
so the base spans multiple frames. A private Unix socket proxy forwards
`SnapshotBegin` to the normal `control watch` client, then pauses reads while
the real CLI asks a question and an explicit TUI action moves the agent tab.

```sh
cargo build --locked -p corvid-tui
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid-tui/tests/claude_qualification.py \
  --corvid target/debug/corvid --scenario snapshot-race
```

The fixture verifies the first published view against a separate snapshot.
Three runs on Linux 7.2.14 sent four base chunks each and reported the tab
move's commit sequence as the ready watermark.
It reattaches in the destination to the same terminal, process, and pending
request. Answering in that TUI resolves the original request ID. The gap and
overflow tests separately verify invalidation and fresh snapshot recovery.

### Backpressure measurements

```sh
cargo test --locked -p corvid-tui --test routing \
  stalled_render_and_snapshot_consumers_release_their_connections -- --exact --nocapture
```

Three debug runs on Linux 7.2.14 with a Ryzen 9 5950X used four live PTYs and
eight unread watch streams over Unix sockets. Each run moved a hidden tab 48
times between workspaces with 120 one-KiB tab labels, taking 820–829 ms.
Across the runs, the largest local ping round trip was 0.95 ms and the largest
input-to-redraw sample on a separate PTY was 40.4 ms. The flooding PTY verified
153–154 cursor position replies from its 32 KiB output bursts before the
render client disconnected at about five seconds. The daemon's peak resident
memory from `/proc` was 22.6–22.8 MiB. These measurements cover the four-PTY
local workload on the recorded host.

### Interactive key latency

```sh
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid-tui/tests/ssh_lifecycle.py \
  --measure-input target/debug/corvid
```

Three debug runs on Linux 7.2.14 with a Ryzen 9 5950X used an 80×24 outer TTY
and a raw inner PTY. Each run recorded 32 samples per row, both locally and
through OpenSSH 10.6p1 on loopback. The inner probe timestamps the received
bytes. The outer probe timestamps receipt of the rendered response. Both use
the same host's monotonic clock. Alt timings start at the suffix write after
an intentional 20 ms gap. The ranges are the smallest and largest per-run
statistics in milliseconds.

| Transport | Esc delay | Input | PTY p50 | Redraw p50 | Redraw p95 |
|---|---:|---|---:|---:|---:|
| Local | 100 | Text | 8.0–8.2 | 41.8–41.9 | 42.2–42.3 |
| Local | 100 | Split Alt | 4.4–4.5 | 21.5–21.6 | 21.9–22.2 |
| Local | 100 | Esc | 107.8–108.2 | 116.6–117.1 | 117.5–117.7 |
| Local | 40 | Esc | 49.4–49.5 | 58.2–58.3 | 58.7–59.0 |
| Local | 40 | Suffix after Esc | 8.1 | 41.7–41.9 | 42.1–42.2 |
| SSH | 100 | Text | 7.9–8.1 | 41.5–41.8 | 42.0–42.3 |
| SSH | 100 | Split Alt | 4.3–20.3 | 21.2–29.2 | 30.6–37.5 |
| SSH | 100 | Esc | 108.0–115.3 | 116.9–124.2 | 133.0–133.2 |
| SSH | 40 | Esc | 49.6–56.3 | 58.8–64.9 | 67.0–75.2 |
| SSH | 40 | Suffix after Esc | 8.0–15.8 | 41.6–41.8 | 42.1–42.4 |

The fixture verified the exact bytes and waited for the standalone Esc redraw
before sending a late suffix. In the split Alt cases the inner PTY received
Esc after the outer probe sent the suffix. The measurements describe one
active PTY and a loopback SSH link. Network delay on a remote deployment adds
to the redraw time and can extend the gap seen by the decoder.

### Version skew reproduction

Build the historical `3d804e1` source with `cargo build --locked -p corvid`.
In a separate copy of the current source, build two test clients with
`PROTOCOL_MAJOR` and `PROTOCOL_MINOR` set to `1, 1` and `2, 0` in
`crates/corvid-protocol/src/session.rs`. Preserve each binary after building
and use separate Cargo target directories for the historical and current
schemas.

```sh
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid-tui/tests/version_skew.py \
  /path/to/corvid-old /path/to/corvid-1.1 /path/to/corvid-2.0
```

The fixture keeps an old TUI attached to a live child while checking both
the major mismatch and a required capability introduced after `3d804e1`.
The newer compatible client reattaches after an explicit detach and reads
the same child's PID and advancing counter. The repository defaults to
protocol 1.0. Additive features advertise individual capabilities.

The `3d804e1` client also attached to a current daemon, read the client list
and snapshot while its TUI was in command mode, and detached with Ctrl-] then d.
It read the new daemon's host identity and snapshot after release profiles were
added to Hello. The current CLI received `UnsupportedCapability` for its
capability query against `3d804e1` and could still read that daemon's identity.

### PID reuse reproduction

The A12 fixture ran on Linux 7.2.14. It uses a private PID namespace and procfs
to set the next child PID before each launch. The two fixture runs use the
same Python interpreter and different markers. The fixture checks that the
first child has exited before it starts the replacement in the same pane.

On Arch Linux with user namespaces, `unshare`, `bwrap`, Python, and `tic`,
run from the repository root.

```sh
cargo build --locked -p corvid-tui
unshare --user --map-root-user bwrap --unshare-pid --as-pid-1 \
  --cap-add CAP_CHECKPOINT_RESTORE --proc /proc --dev /dev --ro-bind /usr /usr \
  --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib /lib64 --dir /run \
  --ro-bind "$PWD/target/debug/corvid" /corvid \
  --ro-bind "$PWD/crates/corvid-tui/tests" /fixtures \
  --ro-bind "$PWD/spikes/terminal/corvid.terminfo" /corvid.terminfo \
  --clearenv --setenv PATH /usr/bin --setenv PYTHONDONTWRITEBYTECODE 1 --chdir / \
  -- /usr/bin/python3 /fixtures/pid_reuse.py /corvid /corvid.terminfo
```

### Unix socket and framing reproduction

The peer fixture ran on Linux 7.2.14 in a private mount and user namespace.
The daemon UID mapped to the invoking host user, and the foreign peer mapped
to a delegated subordinate UID. The fixture opens up only its temporary
sockets to exercise the peer check after a successful connect. It also starts
a foreign listener behind an owner-matching socket file to check the client
side. The original attached PTY remains usable throughout.

On the Arch host used for the test, set `SUB_UID` and `SUB_GID` to one ID from
the user's delegated ranges, then run from the repository root.

```sh
unshare --user --map-root-user --map-users="1:$SUB_UID:1" --map-groups="1:$SUB_GID:1" \
  bwrap --unshare-pid --proc /proc --dev /dev --ro-bind /usr /usr \
  --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib /lib64 --dir /run \
  --ro-bind "$PWD/target/debug/corvid" /corvid \
  --ro-bind "$PWD/crates/corvid-tui/tests" /fixtures --setenv PYTHONDONTWRITEBYTECODE 1 \
  --chdir / -- /usr/bin/python3 /fixtures/socket_peers.py /corvid

cargo test --locked -p corvid-tui --test routing \
  malformed_and_stalled_frames_leave_the_existing_pty_usable -- --exact
cargo test --locked -p corvid-protocol --test framing
```

Protobuf preflight shares the 1,024-item repeated-field budget across
fragments that the decoder can merge into one singular submessage. The
framing tests exercise packed and unpacked fragments at the limit, malformed
headers, and complete render delivery. [Protocol Buffers encoding rules](https://protobuf.dev/programming-guides/encoding/#last-one-wins)

### Reproducible Protobuf Builds

Normal builds generate schema descriptors with `protox` 0.10.0 and Rust code
with Prost 0.14.4. The comparison fixture uses the official `protoc` 36.1
archive as a reference. Verify its SHA-256 before extracting it.
[Protobuf 36.1 release assets](https://github.com/protocolbuffers/protobuf/releases/expanded_assets/v36.1)

| Archive | SHA-256 |
|---|---|
| `protoc-36.1-linux-x86_64.zip` | `c4bc672d9d49214dc8cafdceadf4df92182d6ca8e3ec65a56b2d7de5602669b4` |
| `protoc-36.1-win64.zip` | `390e515cb456e6a978553bdb57baf087b054885077fd6da7f7ff0160279c07d6` |

From a checkout under a trusted directory such as your home directory, run
the A31 fixture on Linux x86-64 with Python 3.11 or newer, `tic`, and the
pinned Rust toolchain. The fixture verifies the compiler archive and builds two
copies of the tracked sources with the lockfile. The first uses the standard
`protox` path with an invalid `PROTOC` setting. The second uses the archived
compiler through `CORVID_SCHEMA_REFERENCE_PROTOC`. It compares the generated
files byte for byte and runs the compiled tests with the compiler unavailable.

```sh
python3 crates/corvid-tui/tests/schema_reproducibility.py /path/to/protoc-36.1-linux-x86_64.zip
```

The official Linux compiler and the Windows compiler running under Wine
also produced byte-identical descriptor sets with source information.
