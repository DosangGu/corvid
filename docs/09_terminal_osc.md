# OSC integration in Desktop and the TUI

**Proposal for review, 2026-11-24.** Start with application progress (OSC 9;4).
A user watching a build in the Tests pane should see the same `42%` in Desktop
and the TUI, including after reattaching from another client. The build can
report progress while its pane is in the background. A report of `100%` says
only what the application last displayed. Agent completion, requests, focus
and input ownership continue to follow the daemon's existing evidence and lease rules.

## Current behavior

| Signal | Behavior in the current tree |
| --- | --- |
| OSC 0/2, terminal title | The terminal host cleans the title and the daemon publishes pane and tab labels. Both clients follow the title. A manual tab name takes precedence. |
| OSC 52, clipboard | The daemon sends a bounded text write on the negotiated interactive writer route. Desktop checks its local setting and focused pane. The TUI checks its setting and route, then emits a fresh OSC 52 to the outer terminal. Both default to off. |
| OSC 133/633, shell markers | The daemon's prompt parser treats A/B as prompt, C as executing and D as unknown. This helps decide shell-history behavior alongside terminal modes and platform process checks. There is no shared shell-phase indicator. |
| OSC 9;4, progress | The pinned Ghostty binding emits `ProgressReport`. Corvid's event loop currently consumes title changes and supported PTY replies and drops the progress report. Neither client can recover it from rendered cells. |
| OSC 7501, program status | Corvid has no end-to-end support today. Desktop's Agents window gets observed Claude and Codex activity and requests from provider hooks. A CLI that only emits OSC 7501 cannot surface its status through that route. |
| Notifications | A provider response can produce an `AgentResponded` message. The TUI can turn it into a bell or an outer-terminal OSC 9. A child application's OSC 9/777 notification has a separate origin and currently has no delivery path to either client. |

See the [terminal engine adapter](../crates/corvid-daemon/src/terminal/screen/ghostty.rs),
[terminal host polling](../crates/corvid-daemon/src/terminal/backend.rs),
[TUI attach client](../crates/corvid-client/src/attach.rs) and
[Desktop inventory watch](../crates/corvid-desktop/src/terminal_session/inventory_watch.rs).

## First slice — progress on a pane

Consume the pinned engine's typed progress event in the terminal host that owns
the PTY. A full or incremental render continues to carry Corvid-composed ANSI.
Both clients can display progress over SSH even when the outer emulator has no
progress support. Each report replaces the live value for that terminal.

| OSC 9;4 state | Proposed presentation |
| --- | --- |
| 0, remove | Clear the indicator. |
| 1, set | Show the supplied integer percentage, including `0%` and `100%`. Keep the last report visible until replaced or cleared. |
| 2, error | Show `Error` with the supplied percentage if present. |
| 3, indeterminate | Show `In progress` without a percentage. |
| 4, paused | Show `Paused` with the supplied percentage, or retain the previous percentage if the application omitted it. |

The wire value has a known state and an optional integer in 0–100. An omitted
percentage differs from zero. Discard an invalid producer update. On a full
snapshot, render a progress field with an unknown state, an out-of-range
percentage or a missing determinate value as absent. Qualify the
pinned engine's interpretation of missing or malformed parameters before
advertising the supported forms. The proposal follows the applications'
progress conventions. [Windows Terminal](https://learn.microsoft.com/en-us/windows/terminal/tutorials/progress-bar-sequences)
and [iTerm2](https://iterm2.com/documentation-escape-codes.html)

Show the indicator beside the automatic or manual tab label in a single-pane
tab. In split tabs, show it beside each source pane's title. The tab picker
can show the single-pane indicator for a background tab. Preserve the active
selection marker and give the pane title priority on narrow views. A compact
view may omit the indicator. Both clients use the same text, including error
and pause states for users who cannot distinguish colors. No gesture is needed
and a report never resizes the PTY or changes the pane receiving input.

### State and delivery

1. Keep one optional progress value in `TerminalScreen` and expose it through
   `Backend::poll` for both local and hosted sessions. Add an optional field to
   the private terminal-host poll response. An older retained host yields absent
   progress. A newer retained host supplies its latest value on the first poll
   after a planned daemon replacement.
2. Keep the normalized type in `corvid-core`. The worker reports changes keyed
   by `TerminalId`. The coordinator looks up the live terminal's current pane
   and discards updates for a retired ID. Extend the full `TerminalState`
   projection with an optional progress message. Field absence clears the client
   value when that terminal entity is replaced. Preserve it through layout moves
   and remove it on exit. A new terminal in the same pane starts empty.
3. Use the same committed value for the daemon's TUI display projection and
   Desktop's existing inventory snapshot/watch. A changed indicator dirties
   visible chrome even when zero cells changed. Apply one update per terminal
   per 100 ms at most, with a trailing flush of the latest value, including a
   clear. Keep one pending value per terminal and skip identical publications.
   Terminal removal cancels the pending flush. The interval is a proposed
   starting budget for the flood test.
4. Scope cached state to host, server epoch and terminal ID. On a watch gap
   replace it from a fresh snapshot. Drop the old epoch on reconnect. A retained
   PTY can repopulate the new epoch from its host. Keep progress in memory only.
   Advertise a proposed `terminal_progress.v1` capability for the new projection.
   Use fresh Protobuf tags and an explicit unspecified enum value. Old clients
   can ignore the field. A new client with an older daemon displays ordinary
   titles. Daemon-composed TUI chrome on a negotiated workspace view needs no
   new client-side OSC interpreter.

## Second slice — OSC 7501 program status and provider hooks

A deploy script waiting for a login and a managed Claude session waiting for
tool permission should each be identifiable from a background pane. OSC 7501
gives any CLI a way to describe its work. A provider hook can additionally
identify the specific question or permission request and its outcome.
This proposed slice shares the terminal projection and reconnect path above.
[Tracking issue #413](https://github.com/DosangGu/corvid/issues/413) ·
[Provider hook coverage](03_provider_qualification.md#corvid-collector)

| Evidence available | What both clients show | Who can change it |
| --- | --- | --- |
| Program report only | A pane indicator such as `App waiting for login`. Pane details identify the reported task and wait reason. | A new report, clear or protocol lifecycle event updates the terminal's records. |
| Provider hook only | The existing agent activity, request count and configured alerts. | The daemon applies the adapter's correlation and evidence rules to each request. |
| Both in the same pane | A pending provider request takes the summary position. App reports remain visible in pane details with their source. | The two records update independently. Only the provider path emits agent alerts or changes the request count. |

For example, a program reports `state=blocked:kind=permission` while Claude
has two open permission requests. Show one pane summary and keep the two request
IDs. Neither the program's `id`, its `app` name nor arrival order identifies a
provider request. An OSC `clear` removes the app report. A matching hook can
settle one request while the other remains pending. This rule works in either
arrival order and keeps app-only indications passive even when hooks are absent.

### Terminal records and recovery

- Parse revision 0.4 in the PTY host. Accept reports without a prior handshake.
  Answer the feature query on the originating PTY with a fixed reply describing
  the supported states and kinds. Qualify the pinned engine's report and reply
  behavior first to avoid a duplicate reply. Negotiate program status separately
  from progress so clients can fall back when a daemon or retained host lacks
  support. [Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status)
- Keep a bounded set of current records per terminal. Each report replaces the
  root or its named record. A named clear removes that subtree and a root clear
  removes all records. A new shell prompt (OSC 133 A) or attached process exit
  expires working and blocked records. Done and error remain available while
  the terminal exists. A full terminal reset clears everything. Screen switches
  and client detach preserve the records. Use the limits and eviction rule in
  the specification, and check the aggregate projection against existing IPC
  budgets before advertising support.
- Give the terminal host's 7501 records precedence over OSC 9;4 in the shared
  status indicator from the first accepted 7501 report until a full reset.
  A late percentage cannot erase a blocked reason or resurrect a cleared task.
  A blocked child is visible even when the root reports idle.
- Project a bounded, source-labelled summary to Desktop's inventory watch and
  the daemon-composed TUI. Keep record details available to the owning pane.
  On reconnect, restore only current records from the same live terminal.
  Discard records when the terminal is retired. Rebuilding a view is silent.
  Treat titles and messages as untrusted plain text, validate limits and text
  before mutation, and keep payloads out of routine logs and disk state.

## Subsequent slices

| Order | User-visible change | Design and delivery |
| --- | --- | --- |
| 3. OSC 133/633 shell phases | Show `At prompt` or `Command running` in pane details on both clients. | Extend the existing incremental prompt parser to share its A/B and C hints. D clears the running hint. Start empty, tolerate repeated markers, and clear on terminal loss or reset. Use an optional terminal-state field and its own capability. Preserve the current foreground checks for shell-history input. Discard command-line, nonce and user-variable payloads. |
| 4. Application notices | With local opt-in, show a short `Terminal app` notice for the active pane in both clients. | Negotiate a separate interactive capability. Consume Ghostty's notification event for the qualified OSC 9/777 forms. Deliver a typed one-shot to the current writer using its full `RouteToken`. Check the route and local setting again at display time. Drop notices during switches, detach, reconnect and queue overflow. Keep them out of snapshots, agent attention, notification history and OS toasts. |

For notices, use a single pending slot per live writer, a maximum of one display
per second and at most 256 UTF-8 bytes of sanitized text overall. Strip control
and bidi-control characters. Bind capture to the writer at PTY read time and
discard partial or pending notices when that route changes. Desktop can show
plain text in its pane chrome. The TUI can use a transient status overlay that
repaints after a frame and restores the status row on dismissal or expiry.
Existing provider response preferences apply only to provider observations.
[VS Code shell integration](https://code.visualstudio.com/docs/terminal/shell-integration)
and [iTerm2 application notifications](https://iterm2.com/documentation-escape-codes.html)

Defer OSC 7 directory hints until a display surface needs them. The daemon
already has a process-observed directory for launch and recovery. OSC 7 can
describe a directory on a nested SSH host, so a future display hint must keep
its provenance and stay out of that launch path. Defer OSC 8 hyperlinks until
link spans can follow cells through redraw, wrapping and history in both clients.
Desktop currently detects visible URLs. A link whose label hides its target
needs that target available for inspection before a user opens it.
[Working-directory reports](https://wezterm.org/shell-integration.html#osc-7-escape-sequence-to-set-the-working-directory)

## Implementation and acceptance

Land each numbered slice with both presentations and its tests. For progress,
start with a fixture that feeds `ESC ] 9 ; 4 ; 1 ; 42 BEL` to a real Corvid
terminal host. Trace it through host polling and the terminal projection before
adding either renderer. Use the following as implementation gates. Runtime
qualification is pending.

| Gate | Required observation |
| --- | --- |
| Ordered parsing (A18, A19, A30) | Run BEL and ST forms through the pinned engine. Split at every byte boundary, join several reports in one PTY read, and interleave printable Unicode. Check all five progress states, missing/invalid fields, cancellation, oversized strings and the next valid report. Cells and PTY replies stay correct. |
| Two clients and two panes (A14, A18, A32) | A background pane reports 42% next to its own title. Compare Desktop and composed TUI output through a move, split, manual rename, narrow viewport and focus change. The other pane, selected writer, agent requests and activity are unaffected. Sending only an OSC clear removes the indicator on both clients. |
| Recovery and skew (A8, A15, A23, A29) | Detach at 42%, update to 75%, then attach from the other client. See 75% on the first synchronized view. Cover watch gaps, retained hosts on replacement, a lost PTY, old/new host and client pairs, and an unknown enum. Closing a pane before a pending flush cannot decorate its replacement. |
| Bounded load (A13) | Flood one PTY with varying percentages while another accepts input. Measure publication rate against the 100 ms budget, pending memory per terminal and second-pane input latency against the same run without reports. Include a slow watcher. After the flood, both clients converge on the final value or clear. |
| Generic CLI status (A8, A18, A19, A32) | With no adapter, report a blocked child under an idle root. Both clients identify the source pane and wait reason. Exercise OSC 9;4 interleaving, subtree and root clear, prompt, exit, full reset and reattach. Fragmented or malformed reports give no partial updates. A feature query gets exactly one reply on the originating PTY. |
| Mixed evidence (A4, A5, A18) | Deliver an OSC wait before and after a matching hook wait in separate runs. Check one summary, at most one alert for that wait and no double counting. With two pending requests, OSC clear, done and error leave both intact. The exact hook outcome settles only its matching request. Repeat with hooks alone. |
| Later slices (A18, A20, A24) | Test incomplete A/B/C/D cycles with ConPTY and a foreground child. Test opted-out, background, stale-route and repeated app notices, including clipboard and provider-event interleaving. No clipboard content, notice text, command line or raw OSC enters routine logs or persisted layout. |

Extend the existing daemon screen fixtures, TUI routing and scripted attach
tests, and Desktop inventory and view tests. Qualify a local Linux PTY and a
Desktop SSH relay against the same fixture, then record native Windows ConPTY
and macOS PTY results before claiming those platforms. See the
[project acceptance matrix](00_project_design.md#15-acceptance-matrix) and the
[current terminal qualification results](01_terminal_qualification.md#libghostty-vt-runtime).
