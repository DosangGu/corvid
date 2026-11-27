# Daemon and Runtime Qualification

> Runtime architecture, Compio workload benchmarks, and reproduction logs.
> For day-to-day daemon usage and setup, see the [Daemon Guide](../guides/daemon.md).

## Compio runtime

The daemon uses Compio for its control, TUI, collector, and upgrade listeners and
their connection tasks. Existing clients use the same CRV1 wire protocol. The
retained terminal hosts, daemon coordinator, timers, shutdown signals, workspace
actors, persistence, and PTY workers also use Compio.

The CLI uses Compio for attach framing, terminal input readiness, stdio relays,
observer hooks, and shell-agent registration. Local control, snapshot watches,
embedded terminal connections, SSH, and WSL use the client's shared Compio IPC
driver with bounded frame queues. The subprocess helper owns child termination
and reaping, including when setup is cancelled.
The companion presentation server and its clients use Compio for private CRV1
connections. Desktop uses Compio for terminal sessions, workspace lookup,
inventory reconnects, and focus requests. Agent Deck uses Compio for its
background worker, presentation clients, and refresh and reconnect deadlines.
These paths are enabled in standard builds on Linux, macOS, and Windows.

**Checks / Core and daemon** verifies the Compio-only protocol build and checks
the Linux workspace dependency graph for Tokio in normal, build, and test
dependencies. Release qualification runs those checks on Windows and macOS.

### Linux workload qualification

Build the default and experimental CLI from revision `295c14c`, which offered
the two daemon backends. The workload
requires `protoc` and creates four real PTYs and eight private snapshot watches.

```sh
cargo build --locked --release -p corvid-tui --bin corvid --target-dir /tmp/corvid-tokio
cargo build --locked --release -p corvid-tui --bin corvid --target-dir /tmp/corvid-compio \
  --features compio-daemon,compio-probe,compio-tui
python3 -B crates/corvid-tui/tests/compio_workload.py \
  /tmp/corvid-tokio/release/corvid /tmp/corvid-compio/release/corvid
```

The report gives process CPU time and summed thread context switches for the
daemon and the four attached clients. It includes p95 Ping round-trip and
input-to-redraw latency. Both binaries run twice in reversed order. On hardware
that exposes a readable Linux package energy counter, pass its `energy_uj` path
as `--energy-counter` to report package joules for each phase.

Two runs per backend on one Linux host with a 12-second idle phase produced
these ranges. Package energy was unavailable on that host.

| Metric | Tokio | Compio |
|---|---:|---:|
| Daemon CPU during idle | 0.13–0.14 s | 0.13–0.14 s |
| Attached client CPU during idle | 0.03–0.04 s | 0.02 s |
| Attached client context switches during idle | 3,619–3,625 | 964–967 |
| Daemon CPU during 20,000 Pings | 0.22 s | 0.18 s |
| Ping p95 | 17.14–17.32 µs | 16.65–16.72 µs |
| Input-to-redraw p95 over 128 events | 16.29–16.43 ms | 16.78–16.79 ms |

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
| Agent history after restart (A23) | With a Claude Code 2.1.318 question open, the fixture acknowledges its alert, waits for persistence, and kills the daemon. The same host starts a new epoch with an empty live inventory. The old question and acknowledgment are visible in history, the question is Unverified, and the old focus returns `EpochMismatch`. The fixture checks that the old run moves from the companion slot to run history. |
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
Split tabs show the active pane title beside the tab strip. Tab titles continue
to update in tab mode.
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

Linux and macOS implement the daemon's internal `ProcessInspector` contract,
with a `ProcessObservation` handle for stat, executable identity/path, bounded
arguments, and verified CWD. Linux retains a procfs directory and queries entries
relative to it. macOS retains a PID/birth pair and rechecks birth identity around
libproc/sysctl observations. Neither handle freezes exec, foreground groups, or
directories into an atomic snapshot. Ownership checks compare birth and image;
broker checks retain one handle and revalidate after reading its metadata.
`BirthIdentity` is an equality token within a boot. Its existing wire field is
still named `start_ticks`; Linux ticks and Darwin microseconds are not converted
into a shared time unit, and no Protobuf schema changes are required.

Observation errors distinguish absence, changed identity, unavailable queries,
invalid data, and size limits. Common daemon policy decides their meaning: a
missing process at shell-agent open or a sampled replacement birth retires that
shell run; a failed subsequent query remains Unknown. The PTY probe preserves
its prior identity on unavailable/reused-PID samples, and requires a matching
sample with exit evidence before reporting Exited. Child-exit evidence remains
authoritative. CWD is published only when its named directory matches the
observed device/inode. Argument queries reject more than 4096 bytes rather than
accepting a command prefix; Darwin's temporary sysctl buffer remains bounded at
256 KiB, with at most 128 arguments and no environment returned.

Initial exec polling stays in common policy: a 250 ms deadline, 2 ms polls,
rejection of the helper/wrong qualified image, and 20 ms of stable observations
for an unqualified image. Interrupted observations reset that stability window.
Scripted-backend tests use a fake clock for these transitions and probe timing.
Native tests cover handle lifetime, CWD identity, bounded argv, and delayed exec;
the delayed-exec fixture uses separate executable copies even on multicall
coreutils systems. These are process-contract tests, not provider qualification.
The TUI shares only a best-effort PID-to-image-path helper through
`corvid-protocol::unix`; it gains no process authority or client-to-daemon
dependency. The daemon retains PTY, liveness, ownership, and probe-budget policy.

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
  --clearenv --setenv PATH /usr/bin --setenv PYTHONDONTWRITEBYTECODE 1 --chdir / \
  -- /usr/bin/python3 /fixtures/pid_reuse.py /corvid
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
