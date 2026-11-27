# Corvid — Architecture Overview

> Rust · shared TUI/GUI workspace behavior · remote Linux daemon · ordinary SSH
>
> Design revision: 2026-11-12. Commands and interfaces described here define the architecture.
> See [terminal qualification](../qualification/terminal.md) and
> [daemon and runtime qualification](../qualification/daemon-and-runtime.md) for implemented paths and runtime evidence.
> See [release and install](../guides/release-and-install.md) for package identity, installation, app updates, and the separate daemon upgrade lifecycle.

## Related Architecture Docs

- [Protocol & IPC](protocol.md) — Discovery, SSH transport, Public protocol v1, framing, schema evolution, snapshot/watch barrier
- [Terminal Runtime](terminal-runtime.md) — PTY/screen pipeline, rendering, writer ownership, input protocols, overload bounds
- [Providers & Focus](providers-and-focus.md) — Provider adapters, AgentMonitor, observation-only hooks, FocusAgent & WezTerm registration
- [Acceptance Matrix](../contributing/coding-and-testing.md#acceptance-matrix) — Acceptance criteria (A1–A30)
- [Shared TUI and GUI Behavior](../contributing/presentation-ux.md#shared-tui-and-gui-behavior) — Presentation UX contract

## 1. Product and support boundary

Corvid is a Rust terminal multiplexer with integrations for Claude Code, Codex, and Antigravity CLI.
The desktop GUI on Windows and Linux and the Linux TUI attach to the same Corvid
multiplexer and aim to provide the same user experience wherever practical.
They share workspace, tab, pane, and focus rules. The Linux TUI runs inside an
existing terminal emulator. The daemon on the Linux execution host owns the
workspaces, PTYs, screen state, and agent observations.
Losing an attached TUI, its working SSH connection, or a local controller must not
stop remote work **while that daemon and the execution host remain alive**.

See §7.7 for the shared behavior contract.

The primary experience is:

1. The user starts multiple interactive agents in remote Corvid terminals.
2. Each newly eligible live agent run gets an empty desktop slot.
3. Cards show working, input needed, response observed, and uncertainty without conflating them.
4. A click selects the run in a specific, already attached Corvid TUI, reached through
   an already open, registered local terminal and its existing working SSH connection.
5. The user answers questions and grants permissions in that terminal.
6. After a local restart, the controller restores slots, synchronizes with the daemon,
   and validates or re-registers the working terminal association.

A controller focus click never creates a terminal window/tab, opens an interactive
SSH session, launches an agent, injects Enter or another answer, or substitutes an
arbitrary client. A separate
background SSH control channel is allowed; it carries state and navigation, not a new
interactive work terminal. Explicit user actions may create workspaces, terminals, and
agents through the TUI or GUI; those actions are outside the external focus operation.

| Surface | Initial target | Boundary |
|---|---|---|
| Server and remote TUI | Linux, one OS user, ordinary SSH | One daemon per user/host; first production slice has one interactive client |
| Working local terminal | Any terminal satisfying the advertised input/VT profile | Windows-to-Linux SSH is a client scenario, not Windows server support |
| Automatic terminal selection | WezTerm, initially on local Windows, with explicit registration | Phase 3 qualification required; manual activation remains available |
| Additional attached clients | One writer per pane; clients can edit different panes independently | Opening an occupied pane transfers its input ownership |
| Desktop | `corvid-desktop` on Windows, Linux (X11 or XWayland), and macOS Apple Silicon, with daemon sessions, tabs, split panes, and a built-in Agents window | Opens a local terminal by default. Remote writers use `corvid terminal-relay` over SSH. A separate control watch supplies layout and observed agent state |
| Agent Deck | `corvid-agent-deck` on Windows, Linux, and macOS Apple Silicon | Displays agent activity and requests exact focus through Desktop or the companion |
| Multiple hosts | Host-qualified IDs and caches from the start | Validate one host first; cross-host operation is later |
| Native Windows daemon and TUI | Windows x64/ARM64 local sessions and Desktop SSH connections | ConPTY and per-user named pipes. See [Windows local use](../guides/daemon.md#run-locally-on-windows) and [Desktop SSH setup](../guides/desktop.md#windows-quick-start) |
| WSL connections | Windows Desktop to an installed Linux distribution's default user | `wsl.exe` stdio carries terminal and control relays. Linux owns PTYs and metadata. Setup and planned replacement share the remote connection implementation |
| Native macOS | macOS 15+ Apple Silicon | Unix PTYs and private sockets; libproc process identity; native Desktop/Agent Deck; unsigned app and DMG packaging. See [macOS support](../guides/desktop.md#macos) for validation boundaries |

This is not a replacement for every terminal emulator feature. Zellij and tmux are
design references, not execution dependencies. There is no public TCP daemon, TLS
service, cloud account, remote approval service, or distributed consensus requirement.

## 2. Locked decisions and their conditions

“Locked” means implementation should follow this direction unless a named phase gate
disproves it. It does not mean a feature is built or compatibility is established.

| Decision | Selected design | Reason / verification condition |
|---|---|---|
| Form and language | Rust desktop and TUI clients of the Linux daemon | Both use the interactive writer protocol and share the daemon layout and workspace operations |
| Process boundary | Client/server foundation with separate terminal hosts for planned replacement | Terminal hosts retain PTYs and VT state while the daemon coordinator is replaced |
| Daemon scope | One daemon per OS user per host, managing all workspaces | One host registry/watch stream and direct cross-workspace focus; larger failure blast radius accepted |
| Core IPC | Same-host Unix stream sockets (`SOCK_STREAM`) | SSH transports terminal/control channels; local IPC still needs authentication, versioning, bounds, and latency tests |
| Layout | Workspace → Tab → N-way tree → Pane → optional Terminal | The daemon creates splits and retains pane identity through selection and detach |
| State authority | Server reducer for domain state; terminal workers for ordered screen state | UI shows evidence, not agent omniscience |
| Focus and keymaps | Per ClientId, resolved on server | No system-wide focused pane or active keymap |
| Input ownership | One writer per pane for clients using pane writers | Retired input is rejected; opening the same pane resumes its existing terminal. |
| PTY size | One canonical size per PTY, derived from its workspace writer | Client viewport is separate; last size survives writer departure |
| Input path | Modal controls plus byte-preserving passthrough first | Legacy bytes do not always identify unambiguous keys |
| Rendering | Server composition and per-client frame state | Ratatui is a composition/diff tool, not a VT engine or recovery protocol |
| Agent observation | Independent provider adapters plus provenance | Hook evidence first; process/OSC heuristics cannot prove work completion |
| Public control format | Protobuf with explicit application versions and capabilities | Shared schema and generated types across daemon, TUI, and companion; independent of Rust domain enums |
| Internal structured messages | The same Protobuf schema family in bounded length-prefixed frames | One structured wire format; Rust evaluation baseline is `prost` / `prost-build` 0.14.4 |
| Render payload | Small Protobuf header plus raw ANSI bytes in a separate frame kind | Custom encoding is limited to framing; no bulk terminal data over the control subscription |
| Persistence | Schema-versioned JSON metadata, atomic replacement | Bincode rejected; see §13 |
| External actions | Read, subscribe, explicit pairing, exact focus, attention acknowledgment | No keystrokes, approvals, spawning, or writer transfer |
| Recovery | Planned daemon replacement retains terminal hosts; unplanned failure restores layout/metadata | Automatic recovery after PTY-owner loss remains deferred |
| Controller slots | Bound to AgentRunId with revisions | Names, pane numbers, and PID reuse cannot redirect clicks |

The former workspace-name socket design is replaced, not retained as an equal option.
Workspace names become labels. Per-workspace daemons would require another host-wide
registry and a cross-daemon client migration path. That complexity is unnecessary for
the initial single-user product. A crash of the selected daemon can affect every workspace;
bounded queues reduce overload risk but cannot provide crash isolation.

## 3. Architecture and state ownership

```text
Local PC                 SSH              Linux execution host H
Existing terminal  ---- with PTY ----->   corvid attach         ---- client.sock --> daemon
Corvid Desktop     ---- without PTY -->   corvid terminal-relay ---- client.sock --> daemon
                  `---- without PTY -->   corvid control --stdio --- control.sock -> daemon
Deck -> companion ----- without PTY -->   corvid control --stdio --- control.sock -> daemon
                                           Agent observation hook -- collector.sock > daemon
```

The working SSH PTY hosts the remote TUI client; the daemon owns separate PTYs for
shells/agents. “Local IPC” refers to processes on H. It does not remove SSH disconnections,
outer-terminal input latency, or controller synchronization concerns.

### 3.1 Responsibilities

| Component | Owns | Does not decide |
|---|---|---|
| A. Terminal runtime | PTY creation, ordered bytes, process handles, exit evidence, resize, VT screen/modes and screen revision | Agent success, approvals, slots |
| B. Agent registry / reducer | IDs, provider normalization, run/turn/request observations, attention records, collector health | Rendering behavior |
| C. Workspace / view management | Layout IDs/mapping, client selection, input leases, view revisions, canonical dimensions | Provider semantics |
| D. Control interface | Handshake, validation, DTOs, queries, snapshots, watches, navigation | Process execution or keystroke injection |
| E. Presentation | TUI/GUI workspace views and desktop cards derived from state; user gestures mapped to shared operations | Independent workspace semantics or authoritative PTY/agent state |
| Local companion | Host connections/caches, synchronization, terminal registrations, slots/pins | Remote authority or a second agent reducer |

### 3.2 Mutation and ordering

One daemon coordinator serializes domain commands and normalized observations. It owns
B and C; these are modules, not separate services. PTY workers own A and maintain strict
byte order per TerminalId. Rendering and persistence consume immutable revisions.

For each accepted domain transaction, the coordinator:

1. Validates epoch, connection role, run owner, deduplication key, and preconditions.
2. Applies the reducer and layout/client selection changes atomically.
3. Recomputes derived badges and attention edges.
4. Increments the epoch's public sequence once and publishes one transaction delta
   containing all affected entities; marks affected views dirty.
5. Queues metadata persistence without waiting for disk or a controller.

Sequence orders **server commits**, not provider causality. PTY chunks and frame updates
have separate per-terminal/per-client revisions; they do not flood the public sequence.
Every mutation of a field exposed in a snapshot produces a public delta. A pane move and
its run-to-location mapping update are never exposed as separate intermediate states.

Input routes and resize commands use tokens checked again by the terminal worker at the
write/resize boundary. A focus transaction waits for the old route to be fenced, not for
remote rendering. This is an asynchronous pending operation: the coordinator continues
processing other terminals, then revalidates all focus preconditions when the fence
completes and commits the view change. A blocked PTY cannot hold the global reducer.
Renderer snapshots can lag PTY output, but never mix view revisions in one frame.
Blocking PTY reads/writes and disk I/O stay off the coordinator.

## 4. Identity and execution location

IDs are opaque, collision-resistant random values, never array indices or display names.
Public references are host-qualified. Ephemeral references also carry ServerEpoch.
An ID may survive in historical storage after it stops being a live address.

| Identifier | Creation and meaning | Persistence / invalidation |
|---|---|---|
| `HostId` | Random installation identity generated on first daemon initialization for this user's host registry | Persist outside runtime directory across daemon/OS restarts. SSH alias and hostname are separate labels. Regenerate deliberately after cloning state; detect conflicting endpoint claims |
| `ServerEpoch` | New random value at every daemon start | Never restored as current; historical rows may retain it. All old live references become invalid |
| `WorkspaceId`, `TabId`, `PaneId` | Created by explicit layout operations | Persist with layout; rename/move retains ID, deletion retires it permanently |
| `TerminalId` | Allocated for each newly opened PTY | Runtime only; history may retain it. A resurrected pane starts empty and gets a new ID only on explicit PTY creation |
| `AgentSessionKey` | Optional `{provider, provider_session_id}` from an adapter | Persist as conversation metadata. Null until supplied; CWD, PID, title, and transcript path are not substitute conversation IDs |
| `AgentRunId` | Allocated for each process/conversation-context incarnation | Live only in creation epoch; retain history. Resume, `/clear`, or explicit context replacement creates a new run even if conversation ID is reused |
| `ClientId` | Assigned on each authenticated TUI attach | In-memory only; disconnect retires it. No reuse via an ID hint on reconnect |
| `ControllerId` | Generated by local companion installation | Persist locally across companion restarts. Neither ClientId nor an authentication secret |
| `RequestId` | Corvid ID for one observed input request under one run/actor | Retain pending/history metadata; never reuse for a new question. Optional provider request/tool ID stored separately |
| `AttentionId` | New attention episode: question, response, or error | Ack targets this record only; new episode always gets a new ID |
| `TurnKey`, `WorkItemId` | Provider IDs when available, otherwise explicitly local observation generations | Scoped by run and actor; local generations cannot supply missing provider ordering |
| `RpcRequestId` | Caller-created protocol correlation/idempotency ID | Separate from domain RequestId; bounded per-controller dedup cache in current epoch |

An AgentRun represents a **context incarnation**, not just a process. Its run lifecycle
can end with `ContextReplaced` while its CLI process and Terminal remain alive. Process
liveness is recorded separately. SessionEnd alone does not establish OS process exit.

Planned daemon replacement may retain a qualified observer's launch association and
original launch epoch after revalidating the same process incarnation and PTY ownership.
It creates a new AgentRunId in the new epoch. Only collector observations have this
explicit bridge; previous requests, attention, client identities and control envelopes
do not become live in the new epoch. A fresh authenticated hook confirms collection.

### 4.1 Location and launch association

The server resolves:

```text
(HostId, ServerEpoch, AgentRunId)
  -> TerminalId
  -> current PaneId -> TabId -> WorkspaceId
```

Each navigable top-level run has one current terminal location. Pane moves keep the
terminal/run identity; deletion makes the location unavailable. A terminal may retain
historical runs, but at most one top-level interactive context is the current navigation
target. Moving a pane is distinct from replacing the PTY in that pane.

The reliable launch path is an explicit Corvid launch action/helper around the real
provider CLI. It registers a process incarnation and random launch association, then
launches the ordinary interactive CLI in the existing PTY. It does not replace the CLI
with an SDK. Names such as `corvid agent launch` are provisional. Interactive TUI and
Desktop clients also provide an **Agent Launcher** (`Ctrl+] a`) that starts a supported
provider directly as a managed PTY in a new tab, split pane, or empty pane, while
preserving the ordinary shell/CLI launch path (`claude`, `codex`, `agy`,
`corvid attach --agent`) as a separate workflow.

An inherited launch token associates events with a terminal/process family, **not with
whichever run currently occupies the pane**. The adapter resolves provider session,
context generation, and actor before applying an event. Unmanaged agents discovered in
a shell start as candidates with limited evidence; ambiguous adoption requires explicit
association rather than a CWD match.

On Linux, process references include boot identity, PID, and start-time evidence;
prefer a retained process handle where available. `/proc` exposes foreground-group and
start-time fields, with access restrictions. PID/command name alone is inadequate.
See [Linux proc_pid_stat(5)](https://man7.org/linux/man-pages/man5/proc_pid_stat.5.html),
checked 2026-11-07. Process identity is auxiliary evidence, never AgentSessionKey.

### 4.2 Parent and child ownership

Record `parent_run_id`, provider actor/subagent ID, and `navigable` separately. A child
with a distinct actor ID gets a child execution record even when it inherits the same
environment and session ID. Child completion closes only that child's work item. It may
update a parent's background-work count; it cannot overwrite the parent's activity,
active turn, or requests. Unknown ownership is quarantined as unassigned evidence.
A nested child is published once its authenticated parent links reach the top-level run.
Phase 2 cards show only independently navigable top-level runs.

### 4.3 Identity examples

| Event | IDs retained | IDs changed / safe result |
|---|---|---|
| Two Claude processes in the same CWD | Possibly identical CWD/provider labels | Distinct TerminalId, AgentRunId, process incarnation; no dedup by path |
| PID reused after exit | Logical pane may remain | New process incarnation and AgentRunId; old observations cannot target it |
| New agent in the same shell/pane | PaneId and possibly TerminalId | New AgentRunId. Old slot captures cannot target the replacement |
| `/clear` or conversation switch | PTY/TerminalId, pane, process may remain | End old context; new AgentRunId on confirmed boundary; use actual provider session ID |
| Same conversation resumed | AgentSessionKey if provider reports same ID | New AgentRunId; optional `resumed_from` link, no inherited approvals |
| Compaction without context replacement | Run/session/terminal | No new run solely because SessionStart source says compact |
| Pane moved across tabs/workspaces | PaneId, TerminalId, AgentRunId | Update current mapping atomically; next focus resolves new location |
| Daemon restart or OS reboot | HostId, saved layout, historical conversation keys | New ServerEpoch; no live TerminalId, AgentRunId, ClientId, or old binding |

If a version reuses session IDs across boundaries and provides no event generation or
causal marker, delayed events may be impossible to assign correctly. Mark that evidence
ambiguous; do not guess from arrival time or route it to the newest run.

## 5. Observed state, requests, and attention

The daemon is authoritative about **what Corvid has observed**. That does not guarantee
that the provider exposed every event or that a monitoring channel never lost one.

The [core agent reducer](../../crates/corvid-core/src/agent/reducer.rs) implements these
transitions for normalized observations. The Claude collector uses the reducer and
`ListAgents` exposes a compact inventory. The control snapshot and its change feed
project requests and attention as separate entries. The outline below describes
the domain fields.

```rust
struct AgentObservationState {
    run_lifecycle: Observed<RunLifecycle>, // Live | Ended(reason) | Unknown
    process_liveness: Observed<ProcessLiveness>, // Running | Exited(status) | Unknown
    activity: Observed<Activity>, // Working | AwaitingPrompt | Interrupted | Error | Unknown
    active_turns: Map<TurnKey, TurnObservation>,
    work_items: Map<WorkItemId, WorkObservation>,
    requests: Map<RequestId, InputRequest>,
    recent_response: Option<Observed<ResponseOutcome>>,
    attention: Map<AttentionId, AttentionRecord>,
    collection: CollectionHealth,
}
// Observed<T>: value, source, source_version, observed_at, provider_at?,
//              evidence_id?, confidence, uncertainty_reason?, coverage_generation.
// InputRequest: owner(run, actor, turn?), provider_request_id?, kind,
//               status(Pending | Resolved | Cancelled | Unverified), evidence.
// AttentionRecord: cause, created_at, acknowledged_at?, related_request_id?.
```

`observed_at` is daemon receipt time in UTC; optional provider timestamps are not a total
clock. Use a monotonic clock for deadlines. Confidence is `confirmed`, `inferred`, or
`unknown`, with an explanation. Preserve provenance per field: a process poll can confirm
liveness without upgrading activity confidence. Response outcomes distinguish response
observed, interrupted, and failed; they do not certify whole-task success.

Collection health is `healthy`, `degraded`, `gap_detected`, `unsupported`, or
`not_registered`, with coverage and last evidence. A healthy, observed AwaitingPrompt
can remain quiet indefinitely. Silence neither completes work nor proves collection
failure. Known delivery failures, ownership ambiguity, incompatible versions, or gaps
invalidate affected claims. If silent loss cannot be detected, disclose that limitation
rather than manufacture a periodic agent-progress heartbeat.

Controller connection state is a separate local dimension:
`Connecting → Syncing → Ready`, or `Offline / Incompatible / Error`. A control heartbeat
proves transport reachability only, not agent liveness or progress.

### 5.1 Display derivation

The first applicable row determines the primary badge. Preserve secondary indicators:
pending count, background work, last-response time, unseen attention, evidence quality.
Use text/icon alongside color. All presentations use the same derivation.

| Priority | Condition | Primary display | Secondary behavior |
|---|---|---|---|
| 1 | Controller not Ready | Connecting / Syncing / Offline / Incompatible | Dim last-known badge/timestamp; retain slot; disable focus until synchronized |
| 2 | Run definitively ended | Ended, with reason | Distinguish process exit from context replacement when retiring cards under §11 |
| 3 | Confirmed pending requests | Needs input (count/kind) | Focus/ack never clears; warning if resolution may have been missed |
| 4 | Unknown run, material collection gap, ambiguous or unverified request/activity | Unknown / Check terminal | Last-known facts remain visible; stale Working/Ready is not current certainty |
| 5 | Current correlated failure/interruption | Error / Interrupted | Independent unseen-attention marker |
| 6 | Confirmed active turn/work item | Working | May also show response observed while background work continues |
| 7 | Current response boundary, no known active work, awaiting prompt | Response observed | No success checkmark; ack removes highlight only |
| 8 | Confirmed next-prompt wait without a new response indication | Ready for prompt | Normal quiet state, no invented request |
| 9 | Insufficient activity evidence | Unknown | A terminal without an agent can separately display Shell |

### 5.2 Reducer invariants and transitions

| Observation or action | Allowed effect | Forbidden inference |
|---|---|---|
| Correlated turn/work start | Add/update its active work; record Working | Clear unrelated requests or response history |
| Explicit request | Upsert request; new AttentionId for a new episode | Replace all questions with one boolean |
| Matching response/result/cancellation | Resolve/cancel that request; recompute remaining work | Resolve other questions because output appeared |
| Current response boundary | Record response, close proven work scopes, derive wait where justified | Process exit or whole-task success |
| Background child finishes | Close child work item; recompute aggregate | Complete a still-working parent or sibling |
| FocusAgent | Change one client's view | Resolve input or implicitly acknowledge an alert |
| AcknowledgeAttention | Ack specified AttentionId, user-wide on that host | Change activity/request status or ack a later question |
| Verified process exit / context end | End affected run; cancel its requests with reason | Transfer requests to new run in same pane |
| Link loss / heartbeat / quiet output | Change transport or retain evidence as appropriate | Agent exit, task progress, completion, or request resolution |

New questions always create new attention even after previous acknowledgment. Dismissing
an uncertain alert changes attention only; its request remains Unverified until evidence
or verified run end explains it. No external `ResolveRequest` or approval method exists
in the proposed v1 contract.

Example: A starts T1, creates R1 and R2, and finishes background job B1. Opening A and
acknowledging R1's alert leaves both requests pending. A matching tool result resolves
only R1, leaving Needs input (1). Delayed T0 Stop can update T0 history at most; it cannot
complete T1 or clear R2.


## 11. Companion and desktop slots

Desktop and TUI start a headless companion as soon as they verify an execution
host. Each companion watches every workspace on that host, independently of
Agent Deck visibility or attached terminal views. It remains running after the
launching client exits. Agent Deck discovers verified host scopes and queries each
local companion for the runs in its synchronized daemon snapshot. An SSH
terminal reattach and Desktop's manual control reconnect both request a fresh
Companion watch. Companion publishes the new connection generation with the
complete verified snapshot. Live runs, attention, and generation-scoped actions
share this state. A previous snapshot can still supply cached status to the
Companion CLI while the watch synchronizes. Companion
applies saved slot order, pins, and suppression to those runs. Deck accepts a
complete set of pages only when their connection generation, daemon epoch,
snapshot sequence, and layout revision agree. The grid and queue share this
per-host view. Hosts that are disconnected or synchronizing contribute no cards.
Deck also polls a separate feed of unread attention. Notification attempts are
deduplicated across Deck windows sharing the same root.

The companion owns host connections, caches, resynchronization, terminal bindings,
and persistent slot layouts. The Deck submits slot actions through the companion
that supplied the displayed run.

Expose a private local IPC endpoint (Unix socket or Windows named pipe restricted to the
current user) for controller presentation, using the same bounded Protobuf framing
with presentation-specific DTOs and permissions. Validate local message sizes and roles.

### 11.1 CLI and desktop validation

On Linux, run `corvid companion slots --count 8` to display slot assignments,
state and evidence, unseen counts, connection status, and exact TUI registrations.
`focus-slot` takes the displayed slot and binding captures. The captures include the
slot revision and the current control connection generation. The CLI uses the same
private Protobuf endpoint as the Corvid Agent Deck. The board shows live and
cached slot state and sends actions for the displayed slot and explicitly selected
terminal. The user can bring the selected terminal forward manually.
See [companion and desktop setup](../guides/desktop.md).

Agents is a built-in Desktop feature, implemented by Corvid Agent Deck in a
separate window and process. It ships and updates with Desktop. The terminal
toolbar provides a compact Agents entry point; Settings uses an Agents category.
Agent activity, notification preferences, and card management belong in the
separate window. Its module/package boundary does not define a public plugin API.

Corvid Agent Deck runs independently and sends focus requests over a private
controller-scoped Desktop endpoint. Desktop routes by host, daemon epoch and run
ID. It prefers a window showing the exact pane, then a window on the same host.
When navigation needs another connection, Desktop confirms the destination pane
before switching the window. Current daemons resolve and validate the live run
when attaching a writer. Older daemons use an exact pane attach with snapshot
verification before and after attach.

When the window switches hosts, Desktop releases the previous writer lease and
retains the authenticated terminal transport when the daemon supports
park_terminal.v1. Resuming acquires a fresh writer route. The cache is bounded
per window, and the daemon reserves interactive connection capacity for new
clients. Desktop reconnects when a parked transport has closed.

The Windows Agents process owns its tray, native notifications, history, startup setting,
and app identity. Its default view is a compact floating deck. Always-on-top mode
keeps it visible; tray mode hides it when focus leaves, with management views
available by expanding the window. Saved grid presets offer 6, 8, 12, or 15 visible
keys in landscape or portrait orientation without changing authoritative slot
assignments. It subscribes to the already-running companion service; closing either
window does not end daemon-owned work. See [Agent Deck](../guides/agent-deck.md) for the
implemented lifetime, Linux limitations, and notification validation boundary.

### 11.2 Deterministic slot policy

A current daemon publishes an immutable creation sequence on each agent record.
The slot allocator sorts records from older peers without this field by run ID
after records with a known sequence. The Linux companion store keeps controller
identity and host configuration in `config.json` and placements in `slots.json`.
Its layout supports up to 64 slots, an overflow queue, and 256 retired run records.

A slot record contains SlotId, optional `(HostId, ServerEpoch, AgentRunId)`, monotonically
increasing slot revision, pin, eligibility order, and removal status. Bind to a run, not
PaneId or a provider session that might be resumed in another execution.

- Admit top-level runs with an owning pane in the synchronized layout and no confirmed
  process exit. Preserve saved order, then append new runs by server creation sequence
  and ID. The first N occupy slots; the rest form the queue.
- Keep a run through work completion, context replacement, acknowledgment, and quiet
  waiting. The run can move between slots and the queue as priority changes.
- Remove a tracked run when a synchronized daemon snapshot confirms its agent process
  exited or its owning pane was closed. Shift every following card forward, then admit
  queued cards at the tail. Pins follow their cards; empty holes are
  not reserved. Increment each changed slot's revision and retain run history.
- Persist pane ownership so history pruning and reconnects cannot lose that association.
  Offline/Syncing/Unknown and missing agent records within the same epoch retain
  saved slots for the Companion CLI. Agent Deck displays only runs present in the
  synchronized snapshot. After synchronization with a new epoch for the same host,
  retire the previous epoch's runs before admitting the current runs. Pins and
  focus captures belong to the original run.
- On a full deck, a higher-priority queued run takes the lowest-priority unpinned
  slot and sends its former occupant to the queue tail. Recent input requests
  appear oldest-first across their unpinned slots. Agent Deck applies the same
  policy across its selected grid and available hosts. See [Agent Deck](../guides/agent-deck.md)
  for the scoring rules.
- Persist after allocation/removal/pin changes, atomically. On restart load placements as
  stale, synchronize, reconcile exact identities, then enable clicks. Never persist queued
  clicks, approval actions, or terminal IDs as trusted permanent registrations.

### 11.3 Click versus reassignment

On key-down/click, capture the displayed tuple `(SlotId, slot_revision, HostId,
ServerEpoch, AgentRunId, binding_revision)`. Never send only a slot number. Freeze a
pressed action's displayed binding until release/cancellation. Companion compares the
captured slot and terminal-binding revisions with its current records before any remote
or local action. A mismatch returns StaleBinding and refreshes the display.

Example: slot 2 shows A at revision 8; A's pane closes and B shifts into revision 9 while the
old press is in flight. The press still targets `(A, 8)` and is rejected. It cannot be
reinterpreted as `(B, 9)`. If A moved panes but its slot/binding did not change, the server
resolves A's new location. Page switching increments displayed binding revisions and
cancels held gestures. The desktop reconnects with a fresh presentation snapshot
and requires a new terminal choice before accepting actions.

## 12. Connection lifecycle and persistence

### 12.1 Live attach and detach

Run `corvid daemon` in the foreground for development or under a configured service.
`corvid control host` and `corvid control ping` exercise the local read-only socket.
The CLI defaults to `$XDG_RUNTIME_DIR/corvid`. Set `--runtime-dir` or
`CORVID_RUNTIME_DIR` to choose an explicit private directory. The daemon stores its
stable HostId under `$XDG_STATE_HOME/corvid` or `$HOME/.local/state/corvid`, and accepts
`daemon --state-dir` for an explicit private state path.

On Linux, `corvid` discovers the user daemon and starts it in the background when
needed. It joins a live terminal in the default workspace or opens the user's
shell when the workspace has no live terminal. The user starts agents by running
their commands in a Corvid shell or by requesting a managed launch in a separate tab.
The background process uses the bundled terminal profile. A control relay alone cannot start the
daemon. Saved layout metadata does not start programs.

Detach or working SSH loss retires that ClientId and its binding, releases its leases,
discards queued input, and keeps last PTY sizes. The daemon continues parsing output and
collecting agent observations with no attached TUI. A subsequent attach gets a new
ClientId, explicit writer claim, current state, and full redraw; it never restores an old
input queue or a previous connection's terminal-mode assumptions.

The writer resume token lives in the client and daemon's memory. The daemon links
it to the current writer's view and terminal, and retains the most recent token
with a detached PTY. A successful resume keeps the token so the same window can
retry if it loses the attach response. An intervening writer replaces the token
for that terminal. Every successful attach still gets a new client ID and input route.

On clean Desktop shutdown, a daemon advertising `graceful_detach.v1` retires
the terminal input route and releases the client's writer claims before
acknowledging detach. Desktop waits briefly for this acknowledgement. Older
daemons and broken connections use the transport's ordinary disconnect path.

Deployment must put the daemon outside the disposable SSH login lifecycle. Prefer an
explicitly installed Linux user service with a verified logout-survival configuration;
when appropriate and authorized by the operator, this can include lingering. A manual
background launch is supported only if the target environment passes the same survival
test. No automatic privileged service/policy change is implied.

Systemd documents session process termination and the separate user-manager/linger
lifecycle in its [logind configuration source](https://github.com/systemd/systemd/blob/main/man/logind.conf.xml),
checked 2026-11-07. Therefore `daemonize()`/setsid alone is not a universal guarantee.
Phase 1 must verify clean logout, abrupt SSH loss, and loss of the last login on the
actual deployment, including service/cgroup policy and runtime-directory lifetime.

### 12.2 Recovery matrix

“Preserved” below assumes the remote daemon/OS is unaffected and deployment passed
§12.1. A consistency snapshot can still contain degraded observations (§9.3).

| Failure | PTY / agent process guarantee | Remote state and client | Local restoration / required action |
|---|---|---|---|
| A. Desktop restart | Preserved | Daemon and working TUI unchanged | Restore the Desktop connection and workspace/tab/pane from client preferences, verify the host and current layout, and attach with fresh input routes; companion pairing remains separate |
| B. Companion restart | Preserved | Working SSH/TUI can remain attached | Load slots/pins as stale, full snapshot, revalidate surviving terminal instance and binding |
| C. Local PC reboot | Preserved remotely | Working TUI disconnects; its ClientId/lease/binding retired | Restore slots, reconnect control; user opens working SSH/TUI and registers its new client; no automatic window creation |
| D. SSH connection loss | Preserved remotely | Working-channel loss detaches TUI; control-only loss merely disconnects watch | Reattach/full redraw for working channel; full snapshot for control; register new TUI client if applicable |
| E. Remote control relay restart | Preserved | Daemon and working client unchanged | Reconnect background stdio SSH and full snapshot; retain/revalidate existing binding |
| F. PTY-owning daemon crash/restart | **Not preserved/recoverable by contract**; an orphan process might survive, but PTY ownership/screen continuity is lost | New epoch; saved layout only, old runs historical/Unknown; old focus rejected | Reconcile stale slots; explicitly create new PTYs/runs; no silent process adoption |
| G. Remote OS reboot | Old processes/PTYs do not survive | HostId/layout may survive; new epoch, no old Live run | Restore the layout and resume a verified conversation when its pane is opened |
| H. Agent process exits | That run ends; shell/PTY may remain | Record exit evidence and cancel its pending requests with reason | Move its card to run history and compact the deck; the next agent gets a distinct run ID even in the same pane |

Planned replacement of an automatically started daemon is implemented on Linux and Windows.
Each terminal host owns its PTY, ordered input, VT parser, and screen state. The daemon
retains authoritative layout, leases, routes, and observations. The shared handoff coordinator
freezes input, detaches clients, waits for metadata persistence, and records host-qualified
terminal identities and layout locations in private runtime storage. A replacement validates
the host identity and live terminal-host protocol before adopting those sessions. The new
epoch requires fresh client identities and writer routes. Older updater clients retain their
idle-only contract through explicit handoff capability negotiation.

Terminal hosts keep their launching implementation until their sessions end. New terminals
use the updated executable. A planned handoff allows the retained Unix agent association or
Windows ConPTY root to keep its original observer epoch. The replacement validates the
hook producer before publishing current activity. Input evidence from the retained
prompt is Unverified until a matching hook confirms or closes it. Automatic recovery
after an unplanned PTY-owner failure remains deferred. Adoption requires the explicit
handoff record and private protocol identity.

### 12.3 Stored data and privacy

| Location | Persist | Never restore as current authority |
|---|---|---|
| Remote identity | HostId, identity schema version | A past epoch or peer authentication |
| Remote state | Layout IDs/tree, settings, last known CWD with source/time, sanitized labels, AgentSessionKey/parent/resume links, historical run IDs, last observation/time/confidence, request-kind/status metadata, attention acknowledgment, schema version | Live PTYs/process handles, current clients/leases, unresolved approvals as actionable current prompts |
| Desktop / TUI navigation | Separate private JSON files with connection target, expected HostId, and last focused tab/pane per workspace in each presentation; Desktop additionally stores an automatic window list and independent startup navigation per internal window ID | ClientId, input routes, writer leases, queued input, process identity, or terminal content |
| Local companion | ControllerId, configured SSH aliases and expected HostId, slots/pins/suppressed runs, stale status cache, schema version | Window/PID/TTY associations without fresh instance/connection validation; queued focus/input |

Do not persist full prompts, questions, answers, output, tool arguments, environment,
authentication material, or pairing codes by default. Metadata can still reveal project
names and session IDs; protect it with per-user permissions/ACLs and redact diagnostic logs.
Question content is not required for a Needs input indicator.

Scrollback persistence is **off** initially. If later enabled explicitly, define a separate
format and policy: proposed 16 MiB per terminal, 128 MiB total, 24-hour retention, private
permissions, and an explicit purge operation. State whether filesystem encryption is
required by the operator; Corvid must not imply encryption it does not provide. Restored
scrollback is labeled historical and cannot reconstruct a live process or pending dialog.

### 12.4 Atomic storage and schema handling

Use schema-versioned JSON and one writer per state file. Validate IDs/references, sizes,
maximum tree depth (32), and file size (16 MiB remote, 2 MiB companion) before loading.
Write a same-directory temporary file with private permissions, flush/fsync it, atomically
rename, then sync the directory where supported. Keep one last-known-good version.
The Windows companion must use a tested replacement/ACL strategy rather than assume
Unix rename/fsync behavior. Persisted sequence is a historical marker, never the next
epoch's starting sequence.

The current Windows implementation uses protected per-user ACLs and a flushed
same-directory replacement. The state ACL, replacement, and recovery tests
passed on Windows build `26300.9711` with NTFS.
[Companion setup and verification](../qualification/platforms.md#desktop-and-companion-verification)

Debounce routine metadata writes up to 1 second; explicit layout save and clean shutdown
wait for persistence within a finite deadline. Status replies identify pending/failed
persistence where relevant. Unexpected power loss may lose the most recent debounced
changes; this is not a durable event journal. Disk failure never blocks PTY processing.

Malformed/truncated files are preserved as bounded quarantined copies and reported;
load the valid backup if available. If layout cannot be recovered, offer a fresh empty
layout without launching commands. If identity is damaged, do not silently generate a
replacement HostId and overwrite evidence; require explicit identity repair/reset.
Unknown newer schemas are left untouched and return an actionable error. Known older
schemas migrate through explicit validated steps into a new file with backup; do not
reinterpret incompatible fields using current Rust defaults.

After daemon restart, persisted Live/Working/Pending values are **last-known historical**
facts. Current run/process state is Unknown (or confirmed exited with separate reboot
proof), and old requests are Unverified/nonactionable. Opening an empty pane with a
verified saved conversation can resume it with a new TerminalId and AgentRunId.
The daemon stores the last verified AgentSessionKey and directory per PaneId, updates
verified context changes, and removes associations with deleted panes. Restore reuses
surviving terminals and reserves empty panes before spawning. Provider confirmation must
match the exact saved session; failures remain pane-specific and explicitly retryable.
Recovery keeps old approvals inactive and validates the stored CWD before launching.
See the
[implemented restore contract](../guides/daemon.md#restore-saved-agent-conversations).

## 13. Dependency decisions and evidence

No dependencies are installed by this document revision. Versions below are concrete
**evaluation baselines**, not a Cargo.lock or a claim that the combination compiles.
All links were checked 2026-11-07. Release metadata and upstream source establish scope,
license, and maintenance evidence; they do not establish Corvid runtime compatibility.
Verify exact release artifacts, transitive licenses, MSRV, and current security/maintenance
status again when creating the first implementation manifest.

| Layer / candidate | Checked baseline and license | Observed scope / maintenance evidence | Decision and qualification gate |
|---|---|---|---|
| PTY | `portable-pty` **0.9.0**, MIT | [Release/API](https://docs.rs/portable-pty/0.9.0/portable_pty/) and [manifest](https://github.com/wezterm/wezterm/blob/main/pty/Cargo.toml); release dated 2025-02-11, upstream PTY tree still received a [2026-09-12 change](https://github.com/wezterm/wezterm/commit/c8c592d19298cb68b8370f4e151f1591333b7f4a), which alone does not establish bug-fix responsiveness | Selected Linux spike baseline. Verify process control, resize, EOF, close/exit, and blocking-I/O integration |
| First screen engine | `vt100` **0.18.1**, MIT | [Release](https://docs.rs/crate/vt100/0.18.1) dated 2026-10-19; [manifest](https://github.com/doy/vt100-rust/blob/main/Cargo.toml), [callbacks](https://github.com/doy/vt100-rust/blob/main/src/callbacks.rs); incremental screen/parser, callbacks for unhandled sequences | Selected Phase 0 candidate for small integration surface. Must pass queries, mode, Unicode, resize, and resource-bound gates; callbacks are not proof all queries are implemented |
| Screen-engine fallback | `alacritty_terminal` **0.26.0**, Apache-2.0 | [Release](https://docs.rs/crate/alacritty_terminal/0.26.0) dated 2026-04-06; [upstream manifest](https://github.com/alacritty/alacritty/blob/master/alacritty_terminal/Cargo.toml) is already 0.26.1-dev; grid/Term and event modules | Evaluate if vt100 fails mandatory behavior; use terminal core, not GUI. Avoid duplicate PTY ownership/event loops |
| Original alternative | `wezterm-term`, upstream manifest **0.1.0**, MIT | [Manifest](https://github.com/wezterm/wezterm/blob/main/term/Cargo.toml) includes multiple workspace/image-related dependencies; standalone release compatibility not verified | Not selected initially. A future evaluation needs an immutable upstream revision and dependency/support review; do not treat manifest version as a stable published contract |
| VT token parser | `vte` **0.15.0**, Apache-2.0 OR MIT | [API](https://docs.rs/vte/0.15.0/vte/) and [manifest](https://github.com/alacritty/vte/blob/master/Cargo.toml); parsing separate from semantics, used by maintained screen-engine candidates; future maintenance not guaranteed | Reuse through selected engine; do not build a new emulator from token callbacks |
| UI composition/diff | `ratatui` **0.30.2**, MIT | [Release](https://docs.rs/crate/ratatui/0.30.2) dated 2026-06-19 and documented frame buffers | Selected; qualify a server-side byte-output backend and per-client recovery; no global renderer |
| Outer terminal handling | `crossterm` **0.29.0**, MIT | [Release/API](https://docs.rs/crate/crossterm/0.29.0), dated 2025-04-05, documented cross-platform controls | Use for TTY setup/output as appropriate; do not depend on lossy key decoding for byte passthrough; Windows portability is not whole-product validation |
| Runtime/local IPC | `tokio` **1.53.3**, MIT | [Release](https://docs.rs/crate/tokio/1.53.3) dated 2026-10-13; [current maintenance policy](https://docs.rs/crate/tokio/latest) lists 1.53.x LTS through September 2027 | Selected evaluation baseline; bounded channels and dedicated blocking PTY workers |
| Control / structured IPC | `prost` + `prost-build` **0.14.4**, `protox` **0.10.0** | Shared proto3 schema and Cargo-managed Rust generation. [Prost API](https://docs.rs/prost/0.14.4/prost/) · [protox](https://docs.rs/protox/0.10.0/protox/) | Selected for explicit wire contracts and schema evolution. Pin the generator and Rust toolchain, compare outputs with the `protoc` reference, and test version skew and bounded decoding. A29–A31 |
| Persistence / provider JSON / diagnostics | `serde` **1.0.229** + `serde_json` **1.0.154**, MIT OR Apache-2.0 | [Serde manifest](https://github.com/serde-rs/serde/blob/master/serde/Cargo.toml), [JSON manifest](https://github.com/serde-rs/json/blob/master/Cargo.toml), [JSON release](https://docs.rs/crate/serde_json/1.0.154) dated 2026-10-11 | Selected for schema-versioned metadata, existing provider payloads, and optional readable exports; independent of generated wire types |
| Former codec | `bincode` **3.0.0** notice; **2.0.1** last functional release listed | [Publisher notice](https://docs.rs/crate/bincode/3.0.0) says maintenance ceased; 3.0.0 intentionally emits a compile error | Rejected for this new project; do not pin 2.0.1 merely to avoid version design |
| Configuration | Same JSON/schema infrastructure initially | Corvid choice, no extra format dependency | TOML/KDL ergonomics may be reconsidered later, not a prerequisite |

Protobuf is selected for a shared schema and explicit evolution rules, without assuming
a measured speed advantage. Custom code handles only the small framing layer; structured
messages use generated Protobuf types and bulk rendering stays raw. Keep application
version/capability negotiation and bound decoding independently of the serialization
library. Do not introduce a parallel JSON wire mode or a custom object codec in the MVP.

The earlier assertion about specific missing ConPTY flags in portable-pty is removed:
no exact Windows release/fork comparison was validated here. Native Windows also needs
named-pipe ACLs, process/job ownership, signals/termination semantics, service/session
lifetime, shell quoting, input, local registration, and terminal behavior testing. It is
not a single `cfg` substitution at the PTY layer.

## 14. Roadmap and phase gates

Acceptance IDs refer to §15. The phase descriptions define scope and exit criteria.
See [terminal qualification](../qualification/terminal.md) for the Phase 0 test results.

The companion and WezTerm adapter have native Windows test results. The
desktop interaction and Windows reboot checks remain open.
Qualify macOS when its separately gated execution work is scheduled.
[Deferred platform verification](../qualification/platforms.md#deferred-platform-verification)

### Phase 0 — disposable technical spike

- **Entry:** This design reviewed; evaluation versions/licenses and the minimum terminal
  profile identified. No architecture depends on a prototype's in-process state.
- **Build scope:** One PTY, selected screen engine, outer-terminal input/output, resize,
  known-state full redraw, and child-query handling. A single process is acceptable only
  for this explicitly disposable experiment.
- **Exit:** A19 compatibility cases pass on recorded Linux/outer-terminal versions;
  engine resource limits and process/EOF behavior are demonstrated; record unsupported
  capabilities and choose the engine. Do not proceed with a broken basic interactive CLI.
- **Failure alternative:** Evaluate alacritty_terminal if vt100 fails required semantics.
  Narrow optional TERM capabilities if honest; do not write a general emulator to rescue
  the candidate. Stop layout work until a viable engine is demonstrated.
- **Excluded:** Complex layouts, desktop, auto-remapping, agent automation.

### Phase 1 — actual client/server foundation

- **Entry:** Phase 0 engine/terminal gate passed and target Linux deployment selected.
- **Build scope:** Daemon, secure sockets, shared proto3 schema, pinned reproducible code
  generation, bounded framing/decoding, handshake/version negotiation, one PTY, exclusive
  writer, input fencing, detach/attach, full redraw, bounded rendering, metadata schema.
- **Exit:** A13–A15, A19–A21, A23–A24, and the Phase 1 portions of A29–A31 pass:
  a long-running process survives working SSH loss/last logout, reattach shows the current
  screen, stale input is rejected,
  mismatched CLI cannot terminate the daemon, and daemon restart is reported as loss of
  live PTYs rather than successful resume. Fragmented/invalid frames stay bounded, unknown
  fields cannot trigger default actions, and schema/code-generation checks are reproducible.
  Document measured queue/latency behavior.
- **Failure alternative:** Correct service lifetime/configuration or explicitly limit the
  deployment; do not declare detach support from a successful daemonize call. Reduce
  rendering frequency or disconnect slow clients, not discard parser input.
- **Excluded:** Full split UX, provider assumptions, Windows server.

### Phase 2 — minimum useful agent management

- **Entry:** Phase 1 survival, rendering, protocol, and input guarantees passed.
- **Build scope:** Several tabs/agents and at least two logical workspaces using the N-way
  model with one pane per tab; Claude adapter; evidence/request/attention model;
  snapshot/watch/focus/ack; local companion core and virtual slots/manual registration.
- **Exit:** A1–A13, A16–A18, A22, A25–A27, and the control-method cases in A29 pass as
  applicable with the exact Claude version recorded. Two same-CWD agents remain distinct;
  A's slot selects only A in a chosen
  existing client; opening a question leaves it pending; snapshot races and local
  companion restart preserve correct state/slots; unsupported correlation is visible.
- **Failure alternative:** Ship explicitly partial adapter capabilities and Unknown where
  evidence is absent. Disable a non-neutral hook. Use virtual slots/manual activation
  until the terminal adapter is qualified; never substitute an SDK workflow or key injection.
- **Excluded:** Rich desktop framework, complete natural-language
  question detection, Codex support claims before qualification, advanced splitting.

### Phase 3 — usable external controllers

- **Entry:** Phase 2 virtual workflow works with stable identities and restart recovery.
- **Build scope:** WezTerm Windows registration/activation, stable slot persistence and
  overflow, desktop presentation, qualified Codex CLI adapter, clear
  offline/uncertain/error states. Reuse the companion and control contract.
- **Exit:** Repeat A1–A18 with supported provider modes. A16/A25/A28 and the presentation
  portion of A29 pass on recorded terminal and Windows versions. A local PC reboot
  restores slots but requires new terminal registration; no click opens a terminal or
  answers anything. Neutral hook failure tests pass for each enabled Codex/Claude event.
- **Failure alternative:** Preserve working virtual/manual path; return ManualRequired for
  unverified foreground activation. If only Codex notify is qualified, report notify-only
  capabilities and report the observed request coverage.
- **Excluded:** App Server replacement of interactive CLI, cross-host automatic routing,
  native Windows daemon, durable PTY crash recovery.

### Later — separately gated extensions

- **Entry:** Phases 1–3 invariants remain covered; each extension has a concrete user need.
- **Scope:** N-way split,
  drag/move and vertical tab-bar UX; automatic keymap profiles/remapping; native Windows
  and macOS execution; opt-in conversation resume; scrollback persistence/search. Consider
  a PTY supervisor only if daemon-crash survival is actually required.
  Converge TUI/GUI workspace operations and session lifetimes (§7.7).
  Desktop and Agent Deck distribution includes [Windows Setup](../guides/release-and-install.md#windows-setup).
- **Exit:** Extension-specific tests plus regression of existing contracts. Input transfers
  must fence stale input/paste, multi-host clicks need
  existing host-specific registrations, and resume must allocate new run IDs.
- **Failure alternative:** Keep the working exclusive-writer/Linux/manual policy and leave
  the extension disabled. Do not weaken ID, input, or persistence guarantees for parity.
- **Excluded:** Mandatory tmux/Zellij, public TCP/cloud accounts, distributed consensus,
  indefinite event logs, autonomous external approvals, and universal emulator parity.


## 16. Risks, remaining questions, and next work

### 16.1 Material risks and mitigations

| Risk | Consequence | Planned response / release gate |
|---|---|---|
| Incomplete provider causality or approval IDs | False input/working/completion claims | Per-field evidence, qualified capabilities, request sets, stale-event rejection, Unknown; A5/A9/A22 |
| Hook neutrality differs by version/event | Monitoring changes execution or blocks a CLI | Static neutral output, finite deadlines, no policy fields, fault-injection qualification; disable failing event; A17 |
| PTY-owning terminal host failure | The affected session loses live process and screen continuity | Explicit matrix, metadata backup, and A23; automatic PTY-host crash recovery remains deferred |
| VT engine/input mismatch | Broken CLI, corrupt screen, lost/misdirected input | Phase 0 gate, TERM honesty, per-client full redraw and input fences; A19/A20 |
| Local terminal identity/OS foreground restrictions | Correct remote view but wrong/no local activation | Explicit pairing, instance/revision checks, separate outcome and manual fallback; A10/A25 |
| Slow consumers and output bursts | Unbounded memory or all agents stalling | Per-terminal/parser budgets, separate control/render queues, disconnect/resync; A13 |
| Schema/generator skew or default-value ambiguity | Wrong action/state despite successful decoding | Shared schema, pinned generation, explicit presence and enum validation, semantic version checks, and bounded framing. A29–A31 |
| Deployment logout policy | Remote work stops despite detach | Verify service/session/runtime-directory behavior on real host; A21 |
| Same-user code and sensitive metadata | Spoofed observations or leaked content | State the single-user trust boundary; private IPC/files, strict roles, no default content retention; A18/A24 |

### 16.2 Open questions with bounded owners

- **Phase 0 engine qualification:** Can vt100's documented APIs satisfy query/mode/Unicode
  behavior and bounded memory without rebuilding its semantic core? If not, evaluate the
  specified fallback before layout work. Pin the successful release and Rust toolchain.
- **Phase 1 measurements:** Tune proposed queue, frame, poll, Esc, and timeout limits using
  recorded interactive/SSH behavior. Keep finite upper bounds; do not call latency solved
  because a Unix socket is local.
  [Backpressure measurements](../qualification/daemon-and-runtime.md#backpressure-measurements)
  [Interactive key latency](../qualification/daemon-and-runtime.md#interactive-key-latency)
- **Phase 1 protocol toolchain:** Qualify and pin an exact `protoc` artifact with
  `prost`/`prost-build` 0.14.4 and the selected Rust toolchain; prove bounded decoding and
  schema-skew behavior.
- **Phase 2 provider qualification:** Record exact Claude release fixtures, prompt/turn
  correlation, request cancellation, subagent ownership, and failure behavior. Document
  events that cannot be reliably reconciled; no promise of general prose-question detection.
- **Phase 3 integration qualification:** Record Codex Hook versus notify coverage and exact
  local terminal versions. Validate Windows instance identity, foreground activation,
  ACLs, and atomic local storage. Choose a desktop framework only after the shared
  companion/virtual-slot contract works.

The former “Closed” shortcuts are replaced by conditional decisions in §2: authentication
needs ownership and peer checks, versions need handshake, latency needs measurement,
and frame diff needs explicit recovery. None is considered solved solely by local IPC.

### 16.3 Next implementation deliverable

Phase 0 qualifies the one-PTY terminal path and records the selected engine and supported
profile. Phase 1 builds the actual daemon and IPC boundary before expanding UI structure.
