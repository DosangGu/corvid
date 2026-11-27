# Terminal Runtime, Rendering, and Input

> Split from the Corvid implementation design. See [Architecture Overview](overview.md) for context,
> [Shared TUI and GUI Behavior](../contributing/presentation-ux.md#shared-tui-and-gui-behavior) for UX parity,
> and [Terminal Qualification](../qualification/terminal.md) for runtime benchmarks.

## 7. Terminal runtime, rendering, and input

### 7.1 PTY and screen pipeline

```text
PTY ordered bytes
  -> incremental VT parser
  -> terminal screen + modes + cursor + bounded scrollback
  -> workspace layout composition for one client's view
  -> per-client frame comparison
  -> framed ANSI output
  -> that client's outer terminal
```

A VT parser recognizes sequences; a terminal engine assigns their meaning and retains
screen/mode state. Ratatui composes grids/widgets and compares frames. These are separate
responsibilities. `vte` explicitly requires a semantic handler; it is not a complete
emulator ([crate documentation](https://docs.rs/vte/0.15.0/vte/), checked 2026-11-07).
Select an existing screen engine under §13 rather than rebuilding one around parser callbacks.

The engine adapter must expose terminal modes, alternate screen, cursor, cell attributes,
resize, and child-query responses. A child's DSR/DA or similar query is answered from its
own emulated terminal state, through its PTY write queue. It is not forwarded indiscriminately
to the outer terminal, whose cursor/grid differs. Only explicitly supported queries get
responses; unsupported capabilities are not advertised. Child OSC title strings become
sanitized display data. A client that negotiates `clipboard_write.v1` can receive
OSC 52 text writes for its current writer route. The daemon sends a structured
event through the interactive stream, and the client applies its local clipboard
policy before writing. When `command_history.v1` is negotiated, the PTY host also
retains up to 256 bounded OSC 133/633 command boundaries (`prompt_row`, execution
start/end anchors, and `wrapped_rows`) on the primary screen across reflow and
scrollback. Both Desktop and the composed TUI expose `Previous command` and `Copy last
command output` via the `Ctrl+] ?` command palette (and `[`/`]`, `c`, `Esc` inside
the history view; TUI copies are bounded by the 64 KiB OSC 52 limit); untrusted
`633;E` command lines and `633;P` properties are discarded, and older peers
ignore the additive history fields.
setting before writing. Events are transient and tied to the view in which the
daemon read the request. Each PTY keeps at most the latest request between polls.
Clipboard read and window-control requests remain disabled.

### 7.2 Rendering and recovery

Each ClientId has its own renderer, viewport, previous frame, cursor/mode tracking,
render generation, frame IDs, and bounded transmit queue. Ratatui's buffer diff is useful
inside this renderer; it does not implement VT compatibility, PTY sizing, reconnect,
or slow-consumer recovery. See [Ratatui Terminal](https://docs.rs/ratatui/0.30.2/ratatui/struct.Terminal.html),
checked 2026-11-07.

On attach, reconnect, viewport resize, render generation change, terminal mode reset,
or detected frame mismatch:

1. Invalidate that client's previous frame.
2. Establish a known outer-terminal state for supported modes/cursor/colors.
3. Send a full current composed frame, then ordered diffs based on that frame.
4. Accept `FrameApplied` only after the client has written the entire frame to its TTY.
   This confirms local delivery, not that the human or physical display has seen it.

The initial slow-render policy is **disconnect that client and require a full redraw on
reattach**. Never drop half an ANSI frame and continue with a later diff. Do not resend
an input queue when reconnecting. A later render-reset protocol may replace disconnect
only if it fences old generations and reconstructs all outer-terminal state.

### 7.3 Writer ownership and canonical sizes

Clients using `pane_writers.v1` hold a writer lease on their selected pane.
Clients can select and write to different panes in the same workspace. Selecting
a pane owned by another client returns `WorkspaceBusy`. An attach without takeover prefers a live pane with no writer. A successful switch releases the source
pane. Bound agent focus can switch to an available pane in the writer's current
workspace. Older clients retain exclusive workspace claims. A legacy claim and
pane writers cannot coexist in the same workspace.

The writer's viewport and Corvid chrome determine layout dimensions and each PTY's
canonical rows/columns. The child process sees exactly one size. Retain the last valid
size when the writer disconnects; use a documented initial size (80×24 before layout
chrome) only for a terminal that has never had a writer. Reject invalid dimensions.

The `status_bar.v1` capability reserves the bottom row when the outer viewport
has at least three rows. The daemon composes the workspace, tabs, and input
mode into the render stream and sets the selected PTY to the remaining size.
Other panes keep their last size until they have a writer. Peers without the
capability use the full viewport. A legacy workspace writer still sizes its
hidden panes.

Desktop and TUI connections are interactive. With `attach.resume.v1`, ordinary
user-initiated attaches use the `session_clients.v1` takeover option to resume an existing
pane directly, selecting the most recently connected writer when no tab or pane
is specified. The default-shell attach may create a shell only when no live
terminal exists. Desktop also uses exact, terminal-ID-checked takeover when the
user selects an occupied pane. With `writer_resume.v1`, Desktop reconnects over SSH
to the exact terminal using the token issued to its previous writer. The daemon
checks the current claim and fences that writer's route before attaching the new
connection. If the pane has a different writer, reconnect returns `WorkspaceBusy`.
If the pane is free, the client can attach to the surviving terminal.
There is no observer role or opt-in writer promotion. Their retired wire values
remain reserved, and older observer requests are rejected.

The worker refuses transfer during a pending paste, fences the previous input
route, resizes the terminal, and installs a new lease generation before the new
client can write. The previous client detaches. External focus controls have no
authority to transfer the lease. A writer can also explicitly disconnect the
other interactive clients viewing its workspace without stopping their PTYs.

Per-client state includes current workspace/tab/pane, mode/keymap, viewport, prior frame,
input-route revision, connection generation, and lease ownership. Foreground/keymap
changes affect only clients viewing that pane, never all clients' focus.

### 7.4 Input protocols and races

Initial input supports UTF-8 and a qualified legacy VT key-sequence profile with bracketed
paste. The client preserves original bytes and incremental sequence boundaries; recognized
keys are optional annotations. `InputChunk` replaces the claim that every input can become
an exact `RawKeyEvent`. Local TTY mode, decoder state, escape timer, connection, and frame
acknowledgments are legitimate client state: “stateless” means **no authoritative domain
state**, not no local state.

Server-side modal resolution is the default. In passthrough, bytes go unchanged to the
selected terminal except for a documented Corvid leader sequence, with a send-literal
escape. In command mode, only documented complete sequences invoke Corvid commands.
Ctrl-C is passed as input to the PTY, not treated as daemon shutdown. Use a configurable
legacy Esc disambiguation delay (initial 100 ms) and document the Esc-versus-Alt ambiguity
under delayed SSH input. Unknown/incomplete sequences remain buffered within bounds,
then take the documented literal path; they never become speculative navigation commands.
Negotiated extended keyboard protocols and automatic remapping are later work.

In command mode, 1–9 selects a live tab by its displayed position.
The shared workspace commands use `w` for the workspace picker, `t` for
the tab list, `p` for spatial pane selection, and `a` for the Agent Launcher
picker (`Enter` new tab, `r` split right, `d` split down, `Esc` cancel).
Agent Launcher is an explicit interactive client gesture, aligned between TUI
and Desktop, and is unavailable to external controllers. The arrow keys after the
leader can also focus a neighboring pane directly. The daemon consumes keys
and paste while a TUI selector is open and resolves the selected identity
before running the ordinary route-fenced switch. `Ctrl+]` then `?` opens
a searchable list of available commands. Repeating the leader sends one
literal leader. Escape cancels the prefix or active selector.

Every input delivery carries `(ClientId, connection_generation, view_revision,
TerminalId, writer_lease_generation, input_sequence)`. The coordinator and PTY worker
reject stale routes. On focus/transfer, fence the old route, discard undelivered stale
input, increment the view/lease generation as applicable, and inform the client. Input
already written before the fence cannot be undone. The client suppresses new forwarding
until it has the new route token; it does not replay keys collected while disconnected.
Tag any partially decoded sequence with its original route too; discard that decoder
buffer on a route change instead of completing it against the new target.

A paste is one bounded transaction. Capture its target at PasteBegin, buffer up to 1 MiB,
and validate the same route at PasteEnd before writing. Send its accepted bytes as one
serialized PTY write batch, with bracketed delimiters only if the child enabled that mode.
Focus/transfer returns `InputBusy` while an accepted paste is being written; it never
redirects a suffix to another agent. A stale, incomplete, oversized, or timed-out buffered
paste is discarded with a visible error. If a PTY fails mid-write, report partial delivery
and prohibit automatic retry. No promise that the application interprets a paste atomically.
Initial paste deadlines are 10 seconds to finish buffering and 5 seconds without write
progress; on write timeout fail the input operation and fence it before allowing navigation.

### 7.5 Bounds and overload

These are initial engineering limits, to be measured at phase gates, not benchmarks:

| Resource | Initial limit / overload action |
|---|---|
| Live PTYs / attached TUIs / external watchers | 32 / 8 / 8 per daemon; reject excess |
| Viewport | Use the reported TTY size. Fit the inner PTY within 36,000 cells and 1,024 columns, leaving any excess area blank. |
| PTY unread user-space queue | 1 MiB per PTY; stop reading only that PTY until parsing catches up |
| PTY writes / terminal replies | Bounded per-terminal queue; prioritize protocol replies at safe batch boundaries; stall/fail that terminal explicitly, never silently discard replies |
| In-memory scrollback | 5,000 rows and 8 MiB per terminal, whichever first; also a 256 MiB aggregate screen/scrollback budget |
| Terminal strings / cell text | 4 KiB OSC/control-string payload, with a separate OSC 52 limit of 64 KiB decoded UTF-8. Up to 64 UTF-8 bytes of combining text per cell. |
| Rendering | Target at most 30 frames/s/client; one pending dirty-screen marker may replace many refresh requests |
| Render transmit | 4 MiB frame, 8 MiB queue/client; 5 seconds without progress disconnects client |
| Domain/collector queue | 4,096 observations or 8 MiB; admission drops observation with gap accounting where possible, never blocks provider |
| Watch backlog | 1,024 deltas or 4 MiB/subscriber; overflow forces resync/disconnect |
| Outstanding requests / history | 256 requests and 1,024 correlation tombstones/run; beyond limit mark coverage incomplete, never invent resolutions; retain a bounded summary |
| Run history / attention | Up to 1,024 historical runs and 256 attention records/run; pruning produces explicit removals and never silently frees an unknown/offline slot |
| Active registry | Up to 1,024 run/child records and 256 work items/run; bound encoded snapshot to 16 MiB; collapse excess observations into an explicit coverage-gap summary |
| Global pressure | Refuse new terminals/subscribers, evict optional scrollback/history first, report degraded observation; do not kill arbitrary agents |

PTY parsing is independent of render cadence. Coalesce screen refreshes, not raw PTY bytes;
otherwise escape/UTF-8 parser state can be corrupted. If CPU cannot keep up, bounded
per-PTY backpressure may slow that producer, which is an explicit limitation. A slow
controller never backpressures PTY parsing or the global reducer. Engine memory bounds
for long sequences/combining text must be verified; unsupported hard bounds fail the
engine qualification gate rather than being claimed by configuration alone.
Size the inner PTY before allocating its screen to keep worst-case redraws within the
frame bound. Reserve the first outer row for navigation when workspace composition is negotiated. Preserve the
available width up to the column budget, then reduce the PTY height to fit the cell budget.

### 7.6 Minimum compatibility suite

Phase 0/1 must exercise fragmented UTF-8 input/output; Korean, wide and combining text;
alternate-screen entry/exit; cursor visibility/movement; indexed/true color; erase and
scroll regions; resize at boundaries; bracketed paste; Ctrl-C; Esc/Alt ambiguity and split
key sequences; child terminal queries/responses; and attach/redraw after output continues.

Use `TERM=xterm-256color` for child PTYs so shells and remote hosts (including
nested SSH hops and containers without custom terminfo entries) handle editing
keys and title updates reliably. The outer client's TERM and the inner PTY's
TERM are separate. IME composition belongs to the outer terminal; Corvid still
must preserve its committed UTF-8. Images, arbitrary clipboard passthrough, and
full emulator parity are outside the initial profile.
