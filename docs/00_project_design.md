# Corvid — AI Coding-Agent Multiplexer: Implementation Design

> Rust · shared TUI/GUI workspace behavior · remote Linux daemon · ordinary SSH
>
> Design revision: 2026-11-09. This document specifies the intended product.
> See [terminal qualification](01_terminal_qualification.md) and
> [daemon foundation](02_daemon_foundation.md) for implemented paths and runtime evidence.
> Commands and interfaces described here define the target.

## Contents

1. [Product and support boundary](#1-product-and-support-boundary)
2. [Locked decisions and their conditions](#2-locked-decisions-and-their-conditions)
3. [Architecture and state ownership](#3-architecture-and-state-ownership)
4. [Identity and execution location](#4-identity-and-execution-location)
5. [Observed state, requests, and attention](#5-observed-state-requests-and-attention)
6. [Provider adapters and AgentMonitor](#6-provider-adapters-and-agentmonitor)
7. [Terminal runtime, rendering, and input](#7-terminal-runtime-rendering-and-input)
8. [IPC and external control contract](#8-ipc-and-external-control-contract)
9. [Snapshot and watch consistency](#9-snapshot-and-watch-consistency)
10. [Exact focus and local terminal registration](#10-exact-focus-and-local-terminal-registration)
11. [Companion and desktop slots](#11-companion-and-desktop-slots)
12. [Connection lifecycle and persistence](#12-connection-lifecycle-and-persistence)
13. [Dependency decisions and evidence](#13-dependency-decisions-and-evidence)
14. [Roadmap and phase gates](#14-roadmap-and-phase-gates)
15. [Acceptance matrix](#15-acceptance-matrix)
16. [Risks, remaining questions, and next work](#16-risks-remaining-questions-and-next-work)

## 1. Product and support boundary

Corvid is a Rust terminal multiplexer with integrations for Claude Code and Codex.
The desktop GUI on Windows and Linux and the Linux TUI attach to the same Corvid
multiplexer and share workspace, tab, pane, and focus rules. Their controls and key bindings may
differ. The Linux TUI runs inside an existing terminal emulator. The daemon on
the Linux execution host owns the workspaces, PTYs, screen state, and agent
observations.
Losing an attached TUI, its working SSH connection, or a local controller must not
stop remote work **while that daemon and the execution host remain alive**.

See §7.7 for the shared behavior contract.
The desktop distribution will later include a Stream Deck integration daemon;
its packaging and lifecycle integration are planned work (§11.4).

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
| Additional attached clients | One writer per workspace; additional attach to an occupied workspace is rejected initially | Read-only observers and handoff are later extensions |
| Desktop | `corvid-desktop` on Windows and Linux (X11 or XWayland), with daemon sessions, tabs, split panes, and the Agent Board plugin | Opens a local terminal by default. Remote writers use `corvid terminal-relay` over SSH. A separate control watch supplies layout and observed agent state |
| Deck | `corvid-deck.exe` on Windows and an experimental USB adapter | Shows agent slots and requests exact focus through the companion. A bundled Stream Deck integration daemon is planned |
| Multiple hosts | Host-qualified IDs and caches from the start | Validate one host first; cross-host operation is later |
| Native Windows daemon and TUI | Windows x64 local sessions and Desktop SSH connections | ConPTY and per-user named pipes. See [Windows local use](02_daemon_foundation.md#run-locally-on-windows) and [Desktop SSH setup](04_companion.md#windows-quick-start) |
| Native macOS daemon | Deferred | Independent process, IPC, permission, service, input, and VT validation required |

This is not a replacement for every terminal emulator feature. Zellij and tmux are
design references, not execution dependencies. There is no public TCP daemon, TLS
service, cloud account, remote approval service, or distributed consensus requirement.

## 2. Locked decisions and their conditions

“Locked” means implementation should follow this direction unless a named phase gate
disproves it. It does not mean a feature is built or compatibility is established.

| Decision | Selected design | Reason / verification condition |
|---|---|---|
| Form and language | Rust desktop and TUI clients of the Linux daemon | Both use the interactive writer protocol and share the daemon layout and workspace operations |
| Process boundary | Real client/server foundation in Phase 1 | Detach survival precedes layout expansion |
| Daemon scope | One daemon per OS user per host, managing all workspaces | One host registry/watch stream and direct cross-workspace focus; larger failure blast radius accepted |
| Core IPC | Same-host Unix stream sockets (`SOCK_STREAM`) | SSH transports terminal/control channels; local IPC still needs authentication, versioning, bounds, and latency tests |
| Layout | Workspace → Tab → N-way tree → Pane → optional Terminal | The daemon creates splits and retains pane identity through selection and detach |
| State authority | Server reducer for domain state; terminal workers for ordered screen state | UI shows evidence, not agent omniscience |
| Focus and keymaps | Per ClientId, resolved on server | No system-wide focused pane or active keymap |
| Input ownership | Explicit workspace writer lease; competing attach rejected | Focus never grants or steals a lease; later observers crop/pad |
| PTY size | One canonical size per PTY, derived from its workspace writer | Client viewport is separate; last size survives writer departure |
| Input path | Modal controls plus byte-preserving passthrough first | Legacy bytes do not always identify unambiguous keys |
| Rendering | Server composition and per-client frame state | Ratatui is a composition/diff tool, not a VT engine or recovery protocol |
| Agent observation | Independent provider adapters plus provenance | Hook evidence first; process/OSC heuristics cannot prove work completion |
| Public control format | Protobuf with explicit application versions and capabilities | Shared schema and generated types across daemon, TUI, and companion; independent of Rust domain enums |
| Internal structured messages | The same Protobuf schema family in bounded length-prefixed frames | One structured wire format; Rust evaluation baseline is `prost` / `prost-build` 0.14.4 |
| Render payload | Small Protobuf header plus raw ANSI bytes in a separate frame kind | Custom encoding is limited to framing; no bulk terminal data over the control subscription |
| Persistence | Schema-versioned JSON metadata, atomic replacement | Bincode rejected; see §13 |
| External actions | Read, subscribe, explicit pairing, exact focus, attention acknowledgment | No keystrokes, approvals, spawning, or writer transfer |
| Crash recovery | Layout/metadata restoration only | PTY-owner crash is not detach; supervisor excluded from MVP |
| Controller slots | Bound to AgentRunId with revisions | Names, pane numbers, and PID reuse cannot redirect clicks |
| Desktop hardware integration | Bundle a Stream Deck integration daemon with the desktop app in a later phase | Reuse companion state and actions; packaging and process lifecycle remain to be implemented |

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
| Stream Deck integration daemon (planned) | Device connection, key images, and button gestures through the companion | PTY ownership, agent state authority, answers, or approvals |

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
with an SDK. Names such as `corvid agent launch` are provisional.

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

The [core agent reducer](../crates/corvid-core/src/agent/reducer.rs) implements these
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
| 2 | Run definitively ended | Ended, with reason | Distinguish process exit from context replacement; release under §11 policy |
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

## 6. Provider adapters and AgentMonitor

### 6.1 Adapter contract and qualification

Claude Code and Codex have separate adapters. Each adapter reports provider version,
execution mode (`interactive_cli`, `notify_only`, or a future `app_server`), adapter
schema revision, supported event kinds, ownership/correlation fields, and capabilities
such as `turn_start`, `response_boundary`, `explicit_question`, `permission_request`,
`request_resolution`, `subagent_identity`, and `revalidate_current_state`.

Capabilities are qualified (`exact`, `partial`, `unavailable`) with reasons; one
`supports_hooks` boolean is insufficient. Unknown versions do not inherit every
capability of the newest documentation. Phase 2/3 must capture fixtures from exact
installed CLI releases and record their versions before claiming runtime support.
No minimum compatible provider version has been established by this design review.
See [provider qualification](03_provider_qualification.md) for captured traces
from Claude Code 2.1.318 and Codex CLI 0.167.0. The agent projection includes the
adapter schema, qualified event and correlation fields, and capability levels.

The proposed adapter produces observations, not mutations:

```rust
struct NormalizedObservation {
    host_id: HostId,
    server_epoch: ServerEpoch,
    launch_association: LaunchAssociation,
    owner: ProviderOwner, // session, actor, context generation; never pane index
    provider_turn_id: Option<String>,
    provider_request_id: Option<String>,
    provider_event_id: Option<String>,
    delivery_id: DeliveryId,
    source: ObservationSource,
    observed_at: Timestamp,
    confidence: Confidence,
    kind: ObservationKind,
}
// ObservationKind examples: ContextStarted, WorkStarted, InputRequested,
// InputResolved, ResponseObserved, ContextEnded, ProcessExited, CollectionGap.
```

Provider payloads are parsed into these explicit internal types. Public DTOs and
persisted schemas have their own versions. Unknown events/fields are counted and
bounded; they cannot accidentally become Working, Complete, or an input command.
Provider-defined Hook/notify JSON stays unchanged at the provider boundary. The helper
converts allowlisted observations into Corvid's bounded Protobuf collector messages;
changing Corvid IPC does not change provider Hook input/output contracts.

### 6.2 Official evidence and proposed mappings

All sources in the following tables were checked **2026-11-07**. Scope `CC-doc` means
Anthropic's current interactive CLI documentation, not a tested binary range. Scope
`CX-doc` means the current official Codex CLI Hook documentation, likewise untested.
Mapping and resolution policies are **Corvid design choices**, not provider guarantees.

Claude evidence: [Hook reference](https://code.claude.com/docs/en/hooks) and
[Hook guide](https://code.claude.com/docs/en/hooks-guide). The reference documents
`session_id`, optional `prompt_id`, actor fields, and tool-specific IDs. In particular,
PermissionRequest omits `tool_use_id`; Notification categories have different meanings.

| Scope | Event / documented meaning | Proposed state effect | Resolution evidence | Missing/uncertain case |
|---|---|---|---|---|
| CC-doc | `SessionStart`: start/resume/clear/compact | Associate session; apply §4 context policy | None; never answers requests | Boundary or actor ambiguity requires quarantine |
| CC-doc | `UserPromptSubmit`: prompt submission | Establish turn evidence; Working if ownership is known | Does not blanket-resolve requests | Submission can still be blocked by another hook |
| CC-doc | `PreToolUse` for `AskUserQuestion` / `ExitPlanMode`: interaction tools | Create typed request keyed by tool ID; expose evidence quality | Matching `PostToolUse`, or explicit matching cancellation/failure | Tool invocation is not proof the dialog remains visible; other hooks may handle it |
| CC-doc | `PermissionRequest`: permission flow | Create a separate permission observation | Only a proven association to subsequent execution/denial; otherwise Unverified | No tool ID in this event: never match by tool name alone |
| CC-doc | `PostToolUse` / `PostToolUseFailure`: tool result/failure | Close matching tool work/request; preserve other work | Same tool ID; failure distinguishes cancellation from answer | Failure does not prove a user approved anything |
| CC-doc | `PostToolBatch`: tool batch completed | Mark matching open questions and plan approvals Unverified when the exact result is missing | Session, prompt, and tool ID | A batch entry alone gives no typed approval or cancellation outcome |
| CC-doc | `Notification`: categorized notification | Allowlist input hints; idle/auth notifications never create generic questions | No generic resolution rule | Unsupported categories remain diagnostic hints |
| CC-doc | `Stop`: response boundary; `StopFailure`: response error | Record response/error for its owner/turn | No blanket request closure | Neither implies process exit; other hooks/background work may continue |
| CC-doc | `SubagentStart` / `SubagentStop`: child boundary | Update child record only | Matching child scope | Inherited environment is not ownership proof |
| CC-doc | `SessionEnd`: context end | End matching context, record reason | Cancel its requests as context ended | Does not replace OS exit evidence |

Use `permission_prompt` as a permission hint, `idle_prompt` as a waiting reminder,
and `auth_success` as neither. Other Notification categories require separate schema
qualification. Free-form assistant prose ending in a question mark is not a reliable
input request. Corvid may show a response notification so the user can inspect it,
but does not promise complete natural-language question detection.

Codex evidence: [CLI Hooks](https://learn.chatgpt.com/docs/hooks),
[notify configuration](https://developers.openai.com/codex/config-advanced/#notifications),
and [App Server approvals](https://learn.chatgpt.com/docs/app-server#approvals).
Current CLI documentation includes lifecycle Hooks; it would be incorrect to describe
all current CLI integrations as notify-only. Hook trust/configuration must be enabled
through the provider's supported user workflow; Corvid does not bypass trust.

| Scope | Event / documented meaning | Proposed state effect | Resolution evidence | Missing/uncertain case |
|---|---|---|---|---|
| CX-doc interactive CLI Hooks | `SessionStart`, `UserPromptSubmit` | Apply context policy; correlate work using supplied turn ID | Not request resolution | Disabled/untrusted hooks mean reduced coverage |
| CX-doc interactive CLI Hooks | `PreToolUse`, `PostToolUse` | Track covered local tools and tool IDs | Matching result closes only its work/request | Hosted tools are outside documented local tool coverage |
| CX-doc interactive CLI Hooks | `PermissionRequest` | Record approval observation under turn | Exact association if established; otherwise Unverified | Documented fields do not supply a general approval request ID |
| CX-doc interactive CLI Hooks | `Stop`, `Interrupt`, `SessionEnd` | Response, interrupted turn, or ended context respectively | Scoped cancellation only when established | Response stop is not process exit; interruption is not success |
| CX-doc interactive CLI Hooks | `SubagentStart`, `SubagentStop` | Child evidence under explicit actor | Child-scoped evidence | Parent session ID can be shared; cannot overwrite parent |
| CLI `notify` configuration | `agent-turn-complete` | Limited response notification with thread/turn metadata | None for approvals/questions | Does not supply all start/request events; TUI notification options are not an external event bus |
| Separate App Server mode, v2 docs | Turn/item notifications and approval/user-input server requests | Potential future rich adapter | `serverRequest/resolved` and correlated request/item outcomes | Not a subscription to an arbitrary running interactive CLI; mode is deferred |

Do not assume Codex exposes `AskUserQuestion` semantics identical to Claude. A validated
local input-tool schema may enable partial detection; otherwise explicit-question
coverage is unavailable. Do not scrape undocumented transcript formats as the default
integration. App Server's request IDs and resolution events cannot be silently attributed
to ordinary CLI Hooks, `notify`, terminal OSC 9, or BEL notifications.

### 6.3 Correlation and ordering rules

- Key work/request transitions by `(epoch, run, actor, provider turn, provider request/tool)`
  where supplied. Preserve provider IDs as opaque strings. Never compare their lexical order.
- Map a request's creation and resolution to the same Corvid RequestId. When a provider
  lacks an ID, allocate a local RequestId with `correlation=partial`. Matching one nearby
  tool name or text is insufficient if parallel/retried requests are possible.
- PermissionRequest without a tool ID may be associated only by a version-qualified,
  unambiguous provider sequence. Without that proof, subsequent output or another tool
  result leaves the request Unverified and prompts inspection. No guessed approval outcome.
- Deduplicate a stable provider event ID where available. A collector delivery ID survives
  a retry of the same delivery. Natural-key upserts make repeated tool/request outcomes
  idempotent. Repeated invocations without a source ID cannot always be distinguished
  from distinct occurrences; never globally deduplicate equal question strings.
- Buffer unmatched resolutions within a bounded window (default 5 seconds, 256 events
  per run). On expiry, record a gap/uncertainty rather than applying to a later request.
  Retain bounded resolved-ID tombstones so a delayed creation cannot reopen a known result.
- An old turn's Stop can update history but cannot overwrite current active work. With
  missing turn ID, continuation ordering, or final-boundary evidence, record only
  `ResponseObserved` and leave current activity Unknown/unchanged as justified. A new
  server sequence number does not make an old provider event causally new.
- Even matching turn IDs may contain multiple work items or continuations. Close only
  the scopes whose end is proven. Other hooks can continue a provider after a Stop;
  awaiting-prompt status must carry that uncertainty when finality is unobservable.
- Run-end tombstones reject delayed events for that run; no event revives an ended run.
  Across context boundaries with insufficient identity, quarantine rather than remap.

### 6.4 Observation-only Hook delivery

`corvid observe --provider claude` accepts hook data from stdin, filters it, and
makes one local Protobuf submission to `collector.sock`. The current adapter
accepts the qualified Claude Code release and checks the launch association,
epoch, provider process incarnation, and the exec-form hook's parent PID.
`corvid observe --provider codex` uses the same collector. For the qualified
Codex release, it also recognizes the launcher's direct internal app-server
child by its installed executable, fixed local-server arguments, and process
incarnation. A change in verified producer invalidates navigation for that run.
See [provider qualification](03_provider_qualification.md) for the enabled scope.

1. Use a fixed installed executable path and fixed arguments. Never interpolate prompt,
   title, CWD, tool arguments, or provider text into shell code.
2. Bound source input to 64 KiB; discard prompt, answer, tool-output, credential, and
   transcript content after extracting allowlisted IDs/kinds. Oversized or invalid input
   becomes a diagnostic/drop, not an exception that changes provider policy.
3. Helper deadline: 100 ms total, including at most 20 ms connecting and 50 ms submitting;
   remaining time covers parsing/cleanup. Configure the provider's supported outer timeout
   to 1 second. The helper uses these application deadlines on Linux. They are
   independent of provider-side process scheduling.
4. Do not wait for the local PC, SSH, a controller, persistence, or a network service.
   No synchronous retry loop or fsync in the hook. The collector may retry an accepted
   delivery internally using the same ID; memory/queue limits still apply.
5. Exit success for observation failure. Normally emit no stdout/stderr. If a qualified
   provider event requires JSON output, emit only a fixed empty object; no decision,
   context, system message, permission, updated input, or continuation fields. Validate
   this neutral behavior per event/version, especially Codex Stop/SubagentStop.
6. Use explicit nonblocking/fail-open provider settings where supported; never configure
   monitoring with `onFailure: block`. Missing executable, timeout, malformed data,
   collector crash, and queue-full behavior must all preserve provider decisions.
7. An explicit managed launch requests observation for that session. For qualified
   releases, pass the observation hooks as additive session settings. Leave provider
   configuration files, trust, permissions, and execution policy under provider and user
   control. If neutrality cannot be established for a version/event, disable that
   observation capability.

Collector failures/gaps degrade state. A dropped hook cannot always report its own
failure; absent durable delivery, exact completeness is not promised. The architecture
chooses bounded observation over blocking the agent. Async hooks, when used, can also
arrive out of order and require the same correlation rules. Provider output behavior
is documented in the [Claude Hook guide](https://code.claude.com/docs/en/hooks-guide)
and [Codex Hook contract](https://learn.chatgpt.com/docs/hooks), checked 2026-11-07;
fault-injection qualification remains required.

### 6.5 Foreground detection is a different signal

AgentMonitor contains an observation collector and a process-identity probe. Foreground
identity can guide later keymap suggestions; it does not establish activity or completion.
OSC shell markers, user variables, process names, and output patterns are hints from
untrusted terminal content. They cannot create a trusted control command, resolve a
request, establish AgentSessionKey, or auto-remap keys in the initial release.

PTY output/SIGCHLD-only probing misses silent foreground changes, shell `exec`, resumed
jobs, grandchildren whose exit the daemon does not reap, and broken shell integration.
Use event-triggered probes plus a bounded periodic safety check: initially every 2 seconds
per tracked terminal, staggered, at most 20 probes/second host-wide. State a larger
worst-case detection delay when the terminal count consumes that budget. Probe process
incarnation and foreground group; debounce burst triggers. Failed probes produce Unknown,
not exit. Absence of output is never a completion timeout.

Corvid-created shells get a private PATH directory for the `claude` and `codex`
commands. A wrapper resolves the real program on the shell's current PATH and
adds session-scoped observers for qualified releases. The daemon binds the launch
to the shell's live PTY using the peer PID, the foreground group, and the executable
image. It accepts observations only after the expected provider takes over that
process. Shell startup files and prompt hooks are left intact. Raw command lines
are not persisted.

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
sanitized display data. Clipboard/window-control OSC is disabled by default.

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

Initial policy is **writer-or-reject per workspace**. An explicit attach/claim grants a
lease only if the workspace is unowned. A second attach to that workspace returns
`WorkspaceBusy`; it is not silently promoted, made writer, or used to resize the PTYs.
A client may explicitly claim multiple workspaces for switching between them. Creating a
workspace in the TUI claims it for that client. FocusAgent does not claim/release leases.

The writer's viewport and Corvid chrome determine layout dimensions and each PTY's
canonical rows/columns. The child process sees exactly one size. Retain the last valid
size when the writer disconnects; use a documented initial size (80×24 before layout
chrome) only for a terminal that has never had a writer. Reject invalid dimensions.

The `status_bar.v1` capability reserves the bottom row when the outer viewport
has at least three rows. The daemon composes the workspace, tabs, and input
mode into the render stream and sets each PTY in that writer's workspaces to
the remaining size. Peers without the capability use the full viewport.

Later, an explicitly requested read-only attach may display the canonical screen using
crop/pad; it cannot resize it. An observer's larger viewport does not create a larger
child screen, and a smaller observer may not see the cursor. Writer handoff is an explicit
TUI action with old/new lease generations and a resize barrier, not an external click.

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

In command mode, 1–9 selects a live tab by its displayed position and w opens a
scrolling list of all tabs in the current workspace. While the list is open, the
daemon consumes keys and pastes locally. Enter resolves the selected tab ID against
the current workspace before running the ordinary route-fenced switch.

Every input delivery carries `(ClientId, connection_generation, view_revision,
TerminalId, writer_lease_generation, input_sequence)`. The coordinator and PTY worker
reject stale routes. On focus/handoff, fence the old route, discard undelivered stale
input, increment the view/lease generation as applicable, and inform the client. Input
already written before the fence cannot be undone. The client suppresses new forwarding
until it has the new route token; it does not replay keys collected while disconnected.
Tag any partially decoded sequence with its original route too; discard that decoder
buffer on a route change instead of completing it against the new target.

A paste is one bounded transaction. Capture its target at PasteBegin, buffer up to 1 MiB,
and validate the same route at PasteEnd before writing. Send its accepted bytes as one
serialized PTY write batch, with bracketed delimiters only if the child enabled that mode.
Focus/handoff returns `InputBusy` while an accepted paste is being written; it never
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
| Terminal strings / cell text | 4 KiB OSC/control-string payload, 64 UTF-8 bytes of combining text per cell; consume syntax safely, limit retained content |
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
frame bound. Reserve the last outer row for the status bar when active. Preserve the
available width up to the column budget, then reduce the PTY height to fit the cell budget.

### 7.6 Minimum compatibility suite

Phase 0/1 must exercise fragmented UTF-8 input/output; Korean, wide and combining text;
alternate-screen entry/exit; cursor visibility/movement; indexed/true color; erase and
scroll regions; resize at boundaries; bracketed paste; Ctrl-C; Esc/Alt ambiguity and split
key sequences; child terminal queries/responses; and attach/redraw after output continues.

Use a proposed `corvid` terminfo/capability profile for child PTYs and advertise only
qualified features. Until that entry is installed, an existing TERM may be used in a
spike only after testing the capabilities it advertises; never claim full xterm support
because common colors render. The outer client's TERM and the inner PTY's TERM are
separate. IME composition belongs to the outer terminal; Corvid still must preserve its
committed UTF-8. Images, arbitrary clipboard passthrough, and full emulator parity are
outside the initial profile.

### 7.7 Shared TUI and GUI behavior

This is the target contract; complete parity is not yet implemented. The GUI
presents the TUI's workspace concepts with graphical controls. Presentation code
maps gestures to shared domain operations, and the execution daemon remains the
authority for accepted state changes and terminal lifetimes.

| Area | Shared behavior |
|---|---|
| Hierarchy and identity | Workspace → Tab → split tree → Pane → optional Terminal; switching or moving a view preserves the existing terminal/run identity |
| Tabs | Creation, ordering, selection, cycling, and closure have the same meaning; graphical tab controls and the TUI tab picker address the same operations |
| Panes | Split direction, pane selection, resize, and closure use common layout rules, including the next selection after a close and handling of the last pane/tab |
| Focus and input | Selection targets an exact pane in one client; writer ownership, route fencing, and literal terminal input follow the same contract |
| Session lifetime | Hiding a tab, closing a presentation window, or detaching is distinct from explicitly closing a terminal; daemon-owned sessions survive client loss |
| Capabilities and limits | Unsupported operations and resource limits are explicit; temporary platform gaps do not define a second set of workspace semantics |

Share layout transitions and their validation through `corvid-core` and the
daemon; keep rendering, mouse interaction, and key bindings in the presentations.
Equivalent action sequences must produce equivalent domain state and lifecycle
outcomes. This does not require identical pixels or keyboard shortcuts.

Both clients can create tabs and split or select panes on the daemon. Explicit
close requires the target workspace's writer lease. The daemon removes the
target and stops its PTYs. Removing the last pane removes its tab, and removing
the last tab removes its workspace. When the active pane closes, the daemon
prefers a surviving live pane in the same tab, then the same workspace, then
another workspace owned by the client. If none is available, the client detaches.
Closing the final workspace leaves a fresh layout without a running shell.
Closing the GUI window releases its writer lease while the daemon keeps its PTYs
alive. Operation-by-operation parity remains subject to A32.

## 8. IPC and external control contract

### 8.1 Discovery, authentication, and SSH

Proposed paths (directories 0700, files/sockets 0600, owner is the daemon UID):

```text
$XDG_RUNTIME_DIR/corvid/client.sock
$XDG_RUNTIME_DIR/corvid/control.sock
$XDG_RUNTIME_DIR/corvid/collector.sock
$XDG_STATE_HOME/corvid/identity.json
$XDG_STATE_HOME/corvid/state.json
```

Use a validated private runtime directory, rejecting unexpected owners, writable parent
components, and symlinks. If XDG_RUNTIME_DIR is unavailable, require an explicitly
configured secure directory; do not silently use a predictable world-writable `/tmp`
path. A per-user instance lock prevents concurrent daemon creation. A stale socket is
removed only after ownership/type/lock checks and a failed liveness handshake; never
unlink a live incompatible daemon's socket.

On Linux, verify connecting UID using `SO_PEERCRED` on stream sockets as well as file
permissions. The client verifies daemon ownership too. This mechanism is documented in
[unix(7)](https://man7.org/linux/man-pages/man7/unix.7.html), checked 2026-11-07.
Separate socket roles constrain accidental misuse: only the TUI role can submit input;
control has no input/spawn/approve methods; collector accepts only observations. This
is a single-OS-user trust boundary, not isolation from malicious code already running
as that same user. A launch token is correlation data, not a substitute for authentication.

The companion connects through the remote control relay.

```text
local companion -> separate background SSH stdio -> remote control relay
                -> control.sock -> daemon

invocation: ssh -T <configured-host-alias> corvid control --stdio
```

`-T` disables PTY allocation; see [OpenSSH ssh(1)](https://man.openbsd.org/ssh.1), checked
2026-11-07. The relay only bridges the control protocol. It neither auto-starts a missing daemon
nor opens an agent/terminal; return `DaemonUnavailable` until the user starts/attaches.
The control SSH channel can reconnect independently of the working SSH terminal.

An explicit Desktop connection may prepare the daemon before opening its
writer and control relays. It probes through SSH, starts an installed binary
when needed, and can transfer a packaged Linux x86_64 binary to the SSH user's
private installation directory. The client verifies the bundle locally and
the remote installer verifies size and SHA-256 before publishing the file
without replacement. The startup lock serializes daemon startup.

Use the user's SSH configuration, known_hosts, keys, and authentication agent. Preserve
host-key checking; changed or unknown keys require the normal user trust workflow.
Background reconnection uses noninteractive authentication and reports `AuthenticationRequired`
instead of looping password prompts.
Do not automatically enable SSH agent forwarding. Bound initial connection to 10 seconds;
retry with jittered backoff capped at 30 seconds, cancellable by the user.

Invoke the local SSH executable with structured arguments and an explicitly configured
host alias (reject aliases beginning with `-`). The remote command is fixed, since SSH
may pass a command through a remote shell. Send all variable data over stdin as framed
Protobuf; never insert workspace names, titles, questions, or IDs into the command string.
Protocol stdout contains only the binary preamble and frames; diagnostics go to bounded
stderr. Unexpected shell startup output is a protocol error, not a control message.
The relay forwards bounded frames without decoding/re-encoding their Protobuf payloads,
so it does not discard fields introduced by another compatible endpoint version.
The transport remains Unix stream sockets locally and ordinary SSH stdio remotely.
Protobuf does not introduce a gRPC/HTTP server or a direct UDP/QUIC endpoint.

### 8.2 Public protocol v1

Use **Protobuf binary messages** for the public control API and internal structured IPC.
Define a shared `proto3` schema package, proposed as `corvid.protocol.v1`, and generate
wire types independently of Rust domain types. Application protocol, provider adapter,
serialization library, code generator, and storage schema versions remain independent.
The reason for this choice is an explicit shared contract and controlled schema evolution;
no throughput or latency improvement has been measured.

An Envelope carries `protocol_major`, `protocol_minor`, optional `request_id`, `host_id`,
`server_epoch`, and a typed `oneof body`. The body identifies the method/result/event;
there is no string-dispatched command or arbitrary serialized Rust enum. Request IDs are
RpcRequestIds, not input RequestIds. Responses echo them; unsolicited events omit them
and include subscription ID and sequence. Only initial Hello and errors before a
successful handshake may omit host/epoch; such errors never update cached host state.

Use opaque bounded strings for IDs and `uint64` for sequences and revisions.
Human-readable JSON exports render those counters as decimal strings.
Use explicit presence wherever absent
and zero/empty have different meanings; enforce required semantic fields after decoding.

The [shared schema](../crates/corvid-protocol/proto/corvid.proto) covers the handshake
and terminal transport. The excerpt shows their fields and the FocusAgent request.

```proto
syntax = "proto3";
package corvid.protocol.v1;

message Envelope {
  uint32 protocol_major = 1;
  uint32 protocol_minor = 2;
  optional string request_id = 3;
  optional string host_id = 4;
  optional string server_epoch = 5;
  optional string subscription_id = 6;
  optional uint64 sequence = 7;
  oneof body {
    Hello hello = 10;
    HelloResult hello_result = 11;
    FocusAgentRequest focus_agent = 20;
    RpcError error = 90;
  }
}
message Hello {
  optional string controller_id = 1;
  repeated uint32 supported_major_versions = 2;
  repeated string required_capabilities = 3;
}
message HelloResult {
  repeated string capabilities = 1;
  Limits limits = 2;
  string writer_policy = 3;
}
message Limits {
  uint32 control_frame_bytes = 1;
  uint32 command_timeout_ms = 2;
}
message FocusAgentRequest {
  string controller_id = 1;
  string client_id = 2;
  string agent_run_id = 3;
  optional uint64 binding_revision = 4;
}
message RpcError {
  string code = 1;
  string message = 2;
  bool retryable = 3;
}
```

The following are **decoded TextFormat illustrations**, not text sent on stdin/stdout:

```textproto
protocol_major: 1
protocol_minor: 0
request_id: "rpc-hello-1"
hello {
  controller_id: "controller-1"
  supported_major_versions: 1
  required_capabilities: "snapshot_watch.v1"
  required_capabilities: "focus_agent.v1"
}
```

```textproto
protocol_major: 1
protocol_minor: 0
request_id: "rpc-hello-1"
host_id: "host-1"
server_epoch: "epoch-7"
hello_result {
  capabilities: "list.v1"
  capabilities: "snapshot_watch.v1"
  capabilities: "focus_agent.v1"
  capabilities: "attention_ack.v1"
  capabilities: "pair_client.v1"
  limits { control_frame_bytes: 262144 command_timeout_ms: 5000 }
  writer_policy: "exclusive_or_reject"
}
```

Control and TUI HelloResult messages include the daemon build's exact release profiles
with adapter revisions, qualified events, correlation fields, and capability levels.
GetCapabilities returns the same description for a control connection. Managed launches
check the executable version and report collection state per run. Socket roles still
restrict operations. A control client identifies its ControllerId during Hello.
Peer authentication follows §8.1.

| Method | Contract |
|---|---|
| `GetHost` | Stable HostId and current ServerEpoch |
| `GetCapabilities` | Negotiated control features, limits, writer policy, and release-qualified provider profiles |
| `GetConnectionStatus` | Live terminal and persistence status, current top-level run coverage, and collector traffic for this epoch. Clients track transport status separately. |
| `ListAgents`, `ListClients`, `ListWorkspaces` | Host-scoped inventories from one revision, with current locations/evidence; no terminal text |
| `SnapshotWatch` | Atomic snapshot plus changes after its barrier (§9) |
| `GetSnapshot` | Consistent one-shot snapshot; must not be followed by a separate unguarded Subscribe |
| `FocusAgent` | Exact existing-client selection under explicit registration (§10) |
| `AcknowledgeAttention` | Idempotent acknowledgment by AttentionId, scoped to the named run/epoch |
| `RegisterClientBinding`, `UnregisterClientBinding` | Explicit single-use pairing or removal of one controller/client association; no input rights |
| `Ping` | Link health only |
| Companion `ListHosts` | Configured endpoints and their remote identities/status; one daemon does not enumerate other hosts |

Snapshots include host/epoch/sequence, workspace trees, terminals and run-location
mappings, current/historical run observations, requests/attention metadata, clients and
lease ownership, relevant binding revisions, and capabilities/collection health.
They exclude PTY bytes, prompts, answer contents, credentials, and pairing secrets.
Default titles are sanitized and bounded; question kind/summary labels suffice without
exporting the full question text.

Errors have `code`, safe `message`, `retryable`, and optional structured `details`.
Use stable codes: `ProtocolMismatch`, `UnsupportedCapability`, `InvalidMessage`,
`MessageTooLarge`, `Unauthorized`, `HostMismatch`, `EpochMismatch`, `DaemonUnavailable`,
`NoAttachedClient`, `AmbiguousClient`, `TargetGone`, `StaleBinding`, `WriterRequired`,
`WorkspaceBusy`, `InputBusy`, `ResyncRequired`, `LimitExceeded`, `Timeout`.
Companion-local errors include `LocalTerminalUnavailable` and `ConnectionRequired`.
An error never triggers an automatic replacement target or interactive connection.

Limits: 256 KiB/control frame including its kind byte, message nesting depth 32,
256 UTF-8 bytes/ID, 1 KiB/display string, and 32 concurrent commands/connection.
Check frame lengths before allocation, configure bounded decoder recursion, and validate
collection counts, strings, and semantic presence before any state mutation. Wire size
alone does not bound decoded allocations; repeated/map fields also obey domain limits.
Malformed Protobuf, truncated frames, or invalid string encoding closes that connection
after a bounded error where possible. Unknown body/enum handling is specified in §8.4.

Command/handshake deadlines are 5 seconds. SnapshotWatch setup follows that deadline;
its subsequent snapshot transfer has the separate §9 limit. Ping every 10 seconds;
30 seconds without any valid inbound frame marks the control link Offline and starts
reconnection, without changing agent liveness.

Negotiate major/minor and capabilities before Attach or any mutation. Same major permits
only documented additive minor changes. A new CLI connecting to an old live daemon may
negotiate a compatible subset or return ProtocolMismatch. It must never kill/restart the
daemon or corrupt work to force an upgrade. The error includes supported versions and
manual upgrade guidance. Installation equality does not imply running-version equality.

An explicit `corvid upgrade` uses an authenticated local upgrade socket, separate
from the control relay. An automatically started daemon advertises
`daemon_upgrade.v1` there. It pauses TUI admission, rejects the request while a
terminal or attach is active, and waits for its metadata to be saved before
approving shutdown. The launcher serializes replacement under the startup lock,
reopens the same state directory, and verifies the same host in a new epoch.
Externally managed daemons are upgraded through their supervisor.

Cache results for up to 4,096 `(ControllerId, RpcRequestId)` commands for 10 minutes per
epoch. Reusing an ID with a different validated semantic command is InvalidMessage. Compare
normalized command fields, not encoded byte equality: wire ordering is not command
identity. Focus/ack retries within that window return the original result, not a new action.
If the client loses a response, resynchronize before deciding whether a fresh explicit action is needed; never persist
and replay click commands after reconnect. A timeout may mean the action committed;
report that uncertainty and check current view state.

### 8.3 Framing and internal TUI protocol

Use a small fixed framing layer over ordered byte streams. Each direction starts with
the four ASCII bytes `CRV1`, denoting framing version 1, then frames of this form:

```text
u32_be frame_length | u8 kind | payload

kind 1: Protobuf Envelope
kind 2: u32_be header_length | Protobuf RenderHeader | raw ANSI bytes
```

`frame_length` includes the kind and payload, excluding its own four bytes. Zero length,
unknown preamble/kind, overflow, truncation, and frames forbidden on the socket role
are errors. Read exactly the stated byte count; neither a socket read nor a Protobuf
field boundary is a frame boundary. Never scan arbitrary bytes for the next magic value
to recover a corrupt stream. Close and re-handshake instead.

Kind 1 is limited to 256 KiB. Kind 2 is permitted only on the daemon-to-TUI path and is
limited to 4 MiB, including kind, header length, header, and ANSI bytes; its header is
at most 16 KiB and must fit inside the outer frame. Check the role-specific length limit
before allocating the payload. RenderHeader contains HostId, ServerEpoch, ClientId, view
revision, render generation, frame ID, optional base-frame ID (absent for a full redraw),
and ANSI byte length. Check the inner/outer lengths agree before emitting any ANSI.
Apply the §7 redraw, generation, and target-validation rules before emitting a frame.

Control, collector, and companion-presentation streams use kind 1 only. Each has a
role-specific set of body types and operations. The collector has an 8 KiB limit
and accepts one observation per authenticated connection. Use ordinary Protobuf
encode/decode within a frame, without adding a
second codec-specific length-delimited wrapper. A future framing change needs separate
negotiation; the fixed bootstrap Hello/Error subset must remain readable before application
version agreement. Unsupported framing fails locally as ProtocolMismatch without touching
the daemon's running work.

Protobuf does not delimit consecutive messages by itself; the length-prefix choice follows
its [streaming guidance](https://protobuf.dev/programming-guides/techniques/#streaming-multiple-messages),
checked 2026-11-07. Do not encode an entire 16 MiB snapshot as one Protobuf message; send
bounded chunks. Keep bulk rendering as raw bytes with a small typed header.

Illustrative message families, not serialized Rust enum layouts:

```rust
// TUI -> daemon, all after negotiated Hello:
Attach { workspace_id, viewport, terminal_capabilities, requested_role }
InputChunk { route_token, input_sequence, bytes }
PasteBegin { route_token, paste_id }
PasteChunk { paste_id, bytes }
PasteEnd { paste_id }
ResizeViewport { client_id, connection_generation, viewport }
SelectView { expected_view_revision, workspace_id, tab_id, pane_id }
ClaimWorkspaceWriter { workspace_id } // explicit user action, never via control socket
FrameApplied { render_generation, frame_id }
Detach

// daemon -> TUI:
Attached { client_id, connection_generation, route_token, writer_leases }
ViewChanged { view_revision, selection, route_token }
RenderFrame { header, ansi_bytes }
AttentionChanged { metadata }
ProtocolError { code, supported_versions }
```

InputChunk and PasteChunk use Protobuf `bytes` fields without base64. Each envelope
fits the control-frame limit; a paste may span several chunks while retaining the §7.4
transaction/target rules. Separate per-client I/O tasks and queues prevent a blocked TUI
from blocking control replies. External watches never carry RenderFrame. EOF or protocol
error cleans up only that connection and its input leases.

The TUI Hello advertises `adaptive_viewport.v1` when the daemon accepts outer dimensions
beyond the legacy 300×120 bounds. Clients report an upgrade requirement before sending
an oversized Attach or ResizeViewport to a legacy daemon.

### 8.4 Schema evolution and code generation

Protobuf parsing compatibility is not semantic compatibility. Keep the handshake,
capability checks, state validation, and explicit errors even when decoding succeeds.

- Assign stable field/enum numbers. Reserve removed numbers and names; never reuse them,
  change an existing field's meaning/type, or move fields between existing oneofs as an
  assumed safe minor update. Prefer additive optional fields with defined absent behavior.
- Unknown optional fields may be skipped at an endpoint. A request with no recognized
  body returns UnsupportedCapability; it never becomes a default command. An unrecognized
  result or state-changing event invalidates that consumer's pending result/cache and
  requires compatible resync/upgrade, rather than silently advancing its sequence.
- Give enums an unspecified zero value. Validate raw numeric values, including unknown
  ones, before deriving state or dispatching actions; never map an unknown value to a
  reassuring default. `prost` enum getters can return a default for unrecognized values,
  so domain conversion must explicitly validate them.
- Validate required application fields, ID scopes, a nonzero protocol major, and presence
  of required preconditions such as binding_revision. Missing does not silently mean
  revision zero; minor version zero is valid.
- Preserve opaque payloads in the SSH relay. Do not rely on a particular generated
  runtime preserving unknown fields after decode/re-encode, or convert live traffic to
  JSON and back. No schema download, runtime reflection, or generic Any dispatch is needed.

Field evolution/presence rules are grounded in the official
[proto3 guide](https://protobuf.dev/programming-guides/proto3/#updating) and the Rust
[prost API](https://docs.rs/prost/0.14.4/prost/), checked 2026-11-07. The handling policies
above are Corvid decisions and require version-skew fixtures, not just successful decoding.

For implementation, keep `.proto` definitions in one shared protocol package; daemon,
TUI, and Rust companion consume generated wire types and explicit domain conversions.
The protocol crate uses `protox` 0.10.0 and `prost-build` 0.14.4. Cargo uses
the same schema descriptor to generate wire types and field tables for bounded
decode preflight. The reproducibility fixture compares both outputs against
the pinned `protoc` 36.1 reference. The preflight
rejects legacy groups, limits known message depth to 32, and bounds repeated fields
before decoding. Apply method-specific semantic limits before executing requests.
Generate into the build output directory and leave generated Rust untouched.
The [protox documentation](https://docs.rs/protox/0.10.0/protox/) describes
descriptor generation for Prost.

JSON remains for schema-versioned persistence/configuration, provider-defined payloads,
and optional human-readable CLI/debug output. A proposed `--json` diagnostic mode decodes
validated messages locally, redacts sensitive fields, and prints text; it is not another
negotiated live wire protocol. Do not serialize generated wire structs directly as the
storage schema.

## 9. Snapshot and watch consistency

The daemon serves `GetSnapshot`, `SnapshotWatch`, and `ListClients` through the
control socket and the stdio relay. Snapshot entities include the layout tree,
live terminal locations, client views and writer leases, agent evidence, requests,
and attention. The negotiated capabilities are included in `SnapshotBegin`.
The daemon sends changed entities and explicit removals in each transaction delta.
The Rust snapshot assembler validates the layout, entity references, scope, and
watermark before exposing a cache. Live transactions validate as a unit and a
failed transaction leaves the previous cache intact.

### 9.1 Atomic barrier

Use a monotonic `u64` public sequence within each ServerEpoch. Initial sequence is zero;
if exhausted, refuse further publication and require controlled maintenance rather than
wrap. v1 reconnect always obtains a new complete snapshot. There is no durable event log,
cross-host order, or distributed consensus.

`SnapshotWatch` is one coordinator operation:

1. At a transaction boundary, capture an immutable domain snapshot at S and install the
   subscriber's bounded delta queue in the same critical section.
2. Release the coordinator; encode/stream that immutable snapshot outside the reducer.
3. Send SnapshotBegin(S), indexed chunks, and SnapshotEnd(S). Maximum total snapshot is
   16 MiB and transfer deadline 30 seconds; each Protobuf chunk/envelope obeys the
   256 KiB control-frame limit.
4. Send queued deltas S+1 onward in order, then a WatchReady watermark W after all deltas
   through W have been sent. Subsequent deltas continue normally.
5. If a queue fills during transfer, abort with ResyncRequired/disconnect. Never label
   an incomplete snapshot ready. Do not hold PTY/reducer progress for slow encoding/writing.

The client builds a staging cache, checks epoch, chunk completeness, entity references,
and contiguous deltas through W, then atomically replaces its visible cache and marks
Ready. A one-shot snapshot becomes current only at S, not a promise that no later changes
exist. Phase 1 can use bounded full copies; copy-on-write is unnecessary before measurement.

### 9.2 Race example

```text
companion -> daemon: SnapshotWatch(request_id=q1)
coordinator: capture snapshot S=40; register watch queue atomically
encoder -> companion: SnapshotBegin(epoch=E, seq=40)
coordinator: A asks question R; commit seq=41; enqueue delta(41)
encoder -> companion: SnapshotChunk(0...n), all from immutable state at 40
coordinator: A moves panes; commit seq=42; enqueue delta(42)
encoder -> companion: SnapshotEnd(seq=40)
watch -> companion: Delta(41, R pending + attention), Delta(42, location mapping)
watch -> companion: WatchReady(through_seq=42)
companion: swap staged cache; A shows R at its new location; mark Ready
```

There is no gap between “read snapshot” and “subscribe.” If 42 arrives twice, ignore the
duplicate after applying it once. If 44 arrives when 43 is expected, stop applying,
mark Syncing, disable navigation, and request a full snapshot. Any epoch change discards
staged data and invalidates live references. Events with an old epoch cannot update a new
cache. v1 watches are unfiltered host feeds for this single user; adding filtered feeds
later must define sequence watermarks so filtering is not mistaken for loss.

### 9.3 Reconnection is not revalidation

During Connecting/Syncing/Offline, cached cards show last-known time and uncertainty.
Keep slots, pins, and unresolved attention after a socket closes.
The remote collector continues while the PC is off, so a question observed during
that time appears in the next snapshot if the daemon remained healthy.

Reading the collector's snapshot reestablishes cache consistency, not missing agent
truth. If collection lost events, invalidate the affected claims and attempt only the
adapter's supported read-only revalidation. Without such support, use Unknown/Unverified
and ask the user to inspect the existing terminal. A current process probe cannot repair
missing question/turn history. A healthy daemon heartbeat cannot repair it either.

## 10. Exact focus and local terminal registration

The daemon supports single-use pairing, unregistration, exact focus, and exact
attention acknowledgment. The TUI displays a 60-second pairing code with Ctrl-] then r.
The control CLI accepts the code on stdin. Client projections expose controller
IDs and binding revisions. Pairing codes are private to the issuing TUI.

### 10.1 FocusAgent transaction

Decoded TextFormat illustration of the Protobuf request (sent as a kind-1
binary frame, not as text):

```textproto
protocol_major: 1
protocol_minor: 0
request_id: "rpc-focus-19"
host_id: "host-1"
server_epoch: "epoch-7"
focus_agent {
  controller_id: "controller-1"
  client_id: "client-4"
  agent_run_id: "run-A"
  binding_revision: 12
}
```

Binding revision identifies the server's controller-to-client registration, not a pane
index, screen revision, or slot assignment. Slot revision is validated by the companion
before constructing this request (§11). Server binding revisions never wrap or get reused.

Under the coordinator, after authentication and protocol validation:

1. Require matching HostId and ServerEpoch; validate binding belongs to ControllerId,
   names the requested current ClientId, and has the expected revision.
2. Require the exact client to be attached. A missing explicit client is NoAttachedClient.
   Companion selection with several registrations and no chosen one is AmbiguousClient;
   v1 daemon FocusAgent itself always requires ClientId.
3. Resolve AgentRunId through the current terminal/layout mapping. Require a current,
   navigable, non-ended run with its terminal present; otherwise TargetGone. No creation.
4. Apply initial writer policy: target client must already hold the destination workspace
   lease. Return WriterRequired if unclaimed, WorkspaceBusy if another client owns it.
   Do not acquire, release, or steal input rights. Later observers may focus read-only
   under an explicitly negotiated capability.
5. Fence its input route (§7.4), then select workspace → tab → pane in one transaction,
   increment that client's view revision, and publish/render the change. Active paste
   returns InputBusy for a fresh user retry. Other ClientIds' view/keymap stay unchanged.
6. Return selection, resolved IDs, new view revision, and commit sequence. Run movement
   before the transaction is followed through current mapping; removal before it is an
   error. Removal after it appears as a later delta, never as a silently substituted run.

A request for the client's current view returns its existing view revision. If the
binding or target changes after the fence, restore the source with a fresh input token.
The controller can replay the same request ID to recover the recorded outcome.

A binding mismatch is StaleBinding, not a hint to retry a different target. After daemon
restart, EpochMismatch is returned before interpreting old run/client IDs. Unsupported
focus capability returns UnsupportedCapability. Selection success means the server view
changed; rendering may still be queued. A separate optional wait for FrameApplied can
report delivery, but cannot prove local OS foreground activation.

The companion returns two independent outcomes:

```text
remote_selection: Applied(view_revision, commit_seq) | Rejected(code) | OutcomeUnknown
local_activation: Activated | PaneSelectedForegroundUnknown | ManualRequired
                  | Unavailable | NotAttempted
```

The daemon cannot return local activation success. Local activation failure does not
undo a successful remote selection or cause another terminal to be chosen.

### 10.2 Initial terminal adapter: explicit WezTerm registration

Choose one local integration first: **WezTerm on Windows, ordinary SSH, explicit pairing**.
No remote WezTerm mux/SSH-domain service is required. Official commands list existing
panes as JSON and activate a named pane; see [list](https://wezterm.org/cli/cli/list.html)
and [activate-pane](https://wezterm.org/cli/cli/activate-pane.html), checked 2026-11-07.
See the [companion verification record](04_companion.md#verification) for the
tested WezTerm and Windows versions.

The Windows desktop pairing flow works as follows.

1. In the already attached remote TUI, the user invokes a Corvid “register controller”
   command. Daemon issues a random one-time code valid for 60 seconds, bound to that
   ClientId/connection generation/epoch. Rate-limit guesses and do not expose the code
   through the ordinary inventory/watch API.
2. The desktop lists verified local WezTerm panes. The user chooses the pane
   containing that SSH TUI and enters the displayed code for the configured host.
   The companion rechecks the selected process, socket file, and pane location
   before submitting the code.
3. Companion sends RegisterClientBinding over the authenticated control link. The daemon
   consumes the code and returns ClientId plus a binding revision. Store the association
   `(local terminal instance, window/tab/pane, host, epoch, client, binding revision)`.
4. On click, verify the local instance/pane still exists, send FocusAgent, recheck the
   same registration, then invoke WezTerm's existing-pane activation by numeric pane ID
   through structured arguments. Never call spawn/split/send-text as a fallback.
5. Report OS window activation separately. If it cannot be confirmed, show ManualRequired
   or PaneSelectedForegroundUnknown and identify the registered window for manual switching.

Window IDs, PID, and TTY names are runtime hints only. The Windows companion keeps
registrations in a volatile registry key scoped to the current logon. After a
companion-only restart it checks the exact remote binding and validates the process
creation time, socket file identity, socket peer, and pane before focus. Require
pairing again after a local PC reboot, a new TUI attach, or a failed identity check.
Do not infer associations from titles, CWDs, SSH aliases alone, or reused numeric IDs.
[Companion restart behavior](04_companion.md#wezterm-on-windows)

A manual fallback lets the user confirm an existing terminal and explicit ClientId,
then performs remote focus while returning ManualRequired locally. This supports the
Phase 2 virtual controller without claiming automatic terminal discovery. With no
attached/registered client, display ConnectionRequired/NoAttachedClient. For another
host, require an existing registration to that host; never reuse a convenient client on
the current host. Paired bindings expire on client disconnect, explicit unregister, or
epoch change. Control-channel loss alone does not destroy a still-live TUI binding.

## 11. Companion and desktop slots

The companion owns host connections, caches, resynchronization, terminal bindings,
and persistent slot layouts. Initially enable one host and retain the same
identity model for later hosts. The desktop app displays slot state and submits
user actions through the companion.

Expose a private local IPC endpoint (Unix socket or Windows named pipe restricted to the
current user) for controller presentation, using the same bounded Protobuf framing
with presentation-specific DTOs and permissions. Validate local message sizes and roles.

### 11.1 CLI and desktop validation

On Linux, run `corvid companion slots --count 8` to display slot assignments,
state and evidence, unseen counts, connection status, and exact TUI registrations.
`focus-slot` takes the displayed slot and binding captures. The captures include the
slot revision and the current control connection generation. The CLI uses the same
private Protobuf endpoint as the Agent Board. The board shows live and
cached slot state and sends actions for the displayed slot and explicitly selected
terminal. The user can bring the selected terminal forward manually.
See [companion and desktop setup](04_companion.md).

The Desktop-launched Agent Board sends focus requests over the private pipes
it inherits from Desktop. Desktop resolves the displayed host, epoch, and run
against its current daemon snapshot. It focuses the pane through its attached
writer connection. The daemon checks the run ID, pane location, and writer
lease at focus commit.

The standalone Windows GPUI deck and direct USB prototype use the same device-independent
slot projection, focus capture, and presentation worker through the private
pipe. A capture holds the displayed run, slot revision,
control generation, and selected binding revision from press through release.
Page or target changes invalidate a capture locally, and the companion validates
the request against its authoritative snapshot. Key activation requests focus.
The GPUI deck view can initialize or reuse a companion and pair a verified
WezTerm pane. A successful pairing selects only its newly registered target.
Alert acknowledgment and slot editing remain explicit actions in the slot
board and CLI. See [Live Stream Deck](04_companion.md#live-stream-deck).

### 11.2 Deterministic slot policy

A current daemon publishes an immutable creation sequence on each agent record.
The slot allocator sorts records from older peers without this field by run ID
after records with a known sequence. The Linux companion store keeps controller
identity and host configuration in `config.json` and placements in `slots.json`.
Its layout supports up to 64 slots, a bounded overflow, and 256 ended records.

A slot record contains SlotId, optional `(HostId, ServerEpoch, AgentRunId)`, monotonically
increasing slot revision, pin, eligibility order, and removal status. Bind to a run, not
PaneId or a provider session that might be resumed in another execution.

- Assign the lowest free unpinned slot when a live, navigable top-level run first becomes
  eligible (confirmed launch/context or first supported activity). On initial snapshot,
  keep valid saved placements, then sort unassigned runs by server creation sequence and
  ID; later multi-host ordering adds HostId as a deterministic tie-breaker.
- Keep a run in its slot through Working, Needs input, Response observed, acknowledgment,
  and quiet waiting. Preserve existing placements as urgency changes.
- On confirmed run end (process exit or confirmed context replacement), clear its binding
  and increment revision; retain an ended record in history. A pinned slot stays reserved
  and empty until explicit reassignment/unpin. An explicit remove suppresses that exact
  run until the user re-adds it; it is not automatically reallocated by the next delta.
- Offline/Syncing/Unknown retains the binding. Missing from a snapshot without an explicit
  end/removal explanation is “check needed,” not permission to reuse the slot. Epoch change
  invalidates focus and retains the historical slot for reconciliation; confirmed remote
  OS reboot can establish prior process termination, otherwise require explicit removal
  or end evidence. Never match an old run to a same-name new run.
- With more runs than slots, append to a stable overflow list in eligibility order. Virtual
  cards can page it. Preserve assigned slots when more urgent runs arrive.
- Persist after allocation/removal/pin changes, atomically. On restart load placements as
  stale, synchronize, reconcile exact identities, then enable clicks. Never persist queued
  clicks, approval actions, or terminal IDs as trusted permanent registrations.

### 11.3 Click versus reassignment

On key-down/click, capture the displayed tuple `(SlotId, slot_revision, HostId,
ServerEpoch, AgentRunId, binding_revision)`. Never send only a slot number. Freeze a
pressed action's displayed binding until release/cancellation. Companion compares the
captured slot and terminal-binding revisions with its current records before any remote
or local action. A mismatch returns StaleBinding and refreshes the display.

Example: slot 2 shows A at revision 8; A ends and B is assigned at revision 9 while the
old press is in flight. The press still targets `(A, 8)` and is rejected. It cannot be
reinterpreted as `(B, 9)`. If A moved panes but its slot/binding did not change, the server
resolves A's new location. Page switching increments displayed binding revisions and
cancels held gestures. The desktop reconnects with a fresh presentation snapshot
and requires a new terminal choice before accepting actions.

### 11.4 Bundled Stream Deck integration daemon — planned

The desktop app will include a background daemon for physical Stream Deck
integration. It handles device discovery, connection loss/reconnect, key images,
and button gestures using the companion's existing slot and exact-focus contract.
The GUI deck and physical keys must show the same slot identities and reject the
same stale captures. The companion owns controller state; the execution daemon
owns terminals and agent observations. Hardware integration grants no additional
input, spawning, or approval authority.

The current `corvid-streamdeck` executable is an experimental standalone USB
adapter. Bundling it as a desktop-managed integration daemon is not implemented.
Process placement, startup/shutdown ownership, behavior when the last window
closes, and login startup are still to be specified. Bundling does not require
the hardware loop to run on the GUI thread or inside the terminal daemon.

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
| A. Desktop restart | Preserved | Daemon and working TUI unchanged | Load current slots from the companion and choose the paired terminal again |
| B. Companion restart | Preserved | Working SSH/TUI can remain attached | Load slots/pins as stale, full snapshot, revalidate surviving terminal instance and binding |
| C. Local PC reboot | Preserved remotely | Working TUI disconnects; its ClientId/lease/binding retired | Restore slots, reconnect control; user opens working SSH/TUI and registers its new client; no automatic window creation |
| D. SSH connection loss | Preserved remotely | Working-channel loss detaches TUI; control-only loss merely disconnects watch | Reattach/full redraw for working channel; full snapshot for control; register new TUI client if applicable |
| E. Remote control relay restart | Preserved | Daemon and working client unchanged | Reconnect background stdio SSH and full snapshot; retain/revalidate existing binding |
| F. PTY-owning daemon crash/restart | **Not preserved/recoverable by contract**; an orphan process might survive, but PTY ownership/screen continuity is lost | New epoch; saved layout only, old runs historical/Unknown; old focus rejected | Reconcile stale slots; explicitly create new PTYs/runs; no silent process adoption |
| G. Remote OS reboot | Old processes/PTYs do not survive | HostId/layout may survive; new epoch, no old Live run | Restore layout/CWD/labels; explicit new shell/agent, optional future conversation resume |
| H. Agent process exits | That run ends; shell/PTY may remain | Record exit evidence and cancel its pending requests with reason | Release unpinned run slot; next agent gets a new run ID even in same pane |

A future PTY supervisor would need to own PTY masters, process identity, screen recovery,
and its own protocol across daemon restarts. That is an additional availability subsystem,
not a free effect of saving layout.json, and is excluded from MVP. Do not auto-adopt an
orphan based on PID, CWD, or a familiar terminal title.

### 12.3 Stored data and privacy

| Location | Persist | Never restore as current authority |
|---|---|---|
| Remote identity | HostId, identity schema version | A past epoch or peer authentication |
| Remote state | Layout IDs/tree, settings, last known CWD with source/time, sanitized labels, AgentSessionKey/parent/resume links, historical run IDs, last observation/time/confidence, request-kind/status metadata, attention acknowledgment, schema version | Live PTYs/process handles, current clients/leases, unresolved approvals as actionable current prompts |
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
[Companion setup and verification](04_companion.md#verification)

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
proof), and old requests are Unverified/nonactionable. Agent-native conversation resume
is a later explicit opt-in action creating a new AgentRunId. No old approval is answered,
no resume command is auto-executed, and a stored CWD is validated before use.

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
See [terminal qualification](01_terminal_qualification.md) for the Phase 0 test results.

The companion and WezTerm adapter have native Windows test results. The
desktop interaction and Windows reboot checks remain open.
Qualify macOS when its separately gated execution work is scheduled.
[Deferred platform verification](04_companion.md#deferred-platform-verification)

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
- **Excluded:** Full split UX, provider assumptions, observers, writer handoff, Windows server.

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
  writer handoff/observers, native Windows daemon, durable PTY crash recovery.

### Later — separately gated extensions

- **Entry:** Phases 1–3 invariants remain covered; each extension has a concrete user need.
- **Scope:** Multiple hosts; explicit read-only attach and writer handoff; N-way split,
  drag/move and vertical tab-bar UX; automatic keymap profiles/remapping; native Windows
  and macOS execution; opt-in conversation resume; scrollback persistence/search. Consider
  a PTY supervisor only if daemon-crash survival is actually required.
  Converge TUI/GUI workspace operations and session lifetimes (§7.7), and include
  the Stream Deck integration daemon in the desktop distribution (§11.4).
- **Exit:** Extension-specific tests plus regression of existing contracts. Observers must
  crop/pad without resizing, handoff must fence stale input/paste, multi-host clicks need
  existing host-specific registrations, and resume must allocate new run IDs.
- **Failure alternative:** Keep the working exclusive-writer/Linux/manual policy and leave
  the extension disabled. Do not weaken ID, input, or persistence guarantees for parity.
- **Excluded:** Mandatory tmux/Zellij, public TCP/cloud accounts, distributed consensus,
  indefinite event logs, autonomous external approvals, and universal emulator parity.

## 15. Acceptance matrix

These are required tests. Record separate implementation evidence for each applicable case.
For runtime/provider cases record exact software versions, setup, evidence observed, and
negative assertions (including no unexpected process/window/input creation).

| ID | Scenario / stimulus | Required observation | Gate |
|---|---|---|---|
| A1 | Two same-provider agents in the same directory | Separate run/process/terminal IDs, state, and slots; no path-based merge | P2, P3 |
| A2 | Click A while B is visible; move A to another tab/workspace first | Current mapping selects exactly A in the specified existing client; no spawn or input; preclaim destination workspace explicitly | P2 |
| A3 | Two clients own different workspaces; focus A in client 1 | Client 2's selection/mode/keymap unchanged; busy destination rejects without lease theft | P2 |
| A4 | Open a screen with a pending question and acknowledge its alert | Request remains pending; only view/attention fields change | P2 |
| A5 | Answer one of two outstanding questions in the terminal | Only matching resolution changes its RequestId; other request remains; missing correlation becomes Unverified | P2, P3 |
| A6 | Response boundary, then later agent process exit | Response alone keeps process/run live and slot allocated; verified exit ends run and releases eligible slot | P2, P3 |
| A7 | PC/control link off while remote agent asks a question | Healthy remote collector retains request; full resync displays it without replaying a prompt/answer | P2, P3 |
| A8 | Mutate request and move pane during snapshot encoding | Immutable S plus contiguous deltas yields latest consistent result; gap/overflow triggers full resync | P2 |
| A9 | Duplicate, delayed, reversed events; old Stop after new turn; `/clear`; child shares environment | No old/child event overwrites new/parent activity; bounded orphan correlation; ambiguity explicitly visible | P2, P3 |
| A10 | No registration/client, stale local window, or ambiguous registered clients | Exact error/ConnectionRequired; no terminal/tab/interactive SSH/agent launch or substitute client | P2, P3 |
| A11 | Remote reboot, then send old focus command | EpochMismatch; old IDs not interpreted, old Live state not resurrected | P1, P2 |
| A12 | Reuse PID and pane display number for a different agent | Old run or slot capture cannot target the replacement. Process incarnation check prevents reuse confusion | P2 |
| A13 | High PTY output plus stalled state watcher/render client | Bounded queues/memory; watcher resync or render disconnect; ordered parser bytes and unrelated agents continue within host resource budget | P1, P2 |
| A14 | Attach clients with different sizes to one workspace | Initial competing attach rejected; no resize. Later observers crop/pad canonical writer screen; no writer keeps last size | P1, Later |
| A15 | Updated CLI meets an old running daemon | Compatible negotiation or clear ProtocolMismatch; daemon/processes stay alive | P1 |
| A16 | Slot reassigned or page changed during press | Captured run/revision rejected as stale, never redirected to new occupant; no replay after reconnect | P2, P3 |
| A17 | Corrupt/newer-schema storage, unsupported events, hook timeout/crash/missing helper/full queue | Preserve/recover files safely; no guessed Live state, policy change, blocked agent, or unlimited diagnostics | P1–P3 |
| A18 | Agent emits forged OSC/title/question text containing shell/control syntax | Display sanitization; no shell interpolation, focus, approval, or trusted lifecycle mutation | P2 |
| A19 | Fragmented UTF-8, Korean/wide/combining, alternate screen/cursor/color/erase/scroll, resize, paste, Ctrl-C/Esc/Alt, queries, TERM profile | Qualified behavior matches advertised capabilities; full redraw reconstructs current visible state | P0, P1 |
| A20 | Focus/resize/disconnect while keys or paste are buffered; later lease handoff | Fenced old tokens rejected; accepted paste stays at original target; partial delivery not retried; no stale-key replay | P1, Later |
| A21 | Long-running child; abrupt SSH disconnect and clean last logout | Same daemon/PTY/process survives on documented deployment; reattach observes continued progress | P1 |
| A22 | Collector gap followed by full control resync | Cache consistent but affected activity/request remains Unknown/Unverified until actual evidence | P2 |
| A23 | Kill PTY-owning daemon mid-work, restart with saved layout | New epoch and empty PTY slots; historical state labeled; no process continuity claim or automatic resume | P1 |
| A24 | Unauthorized UID, unsafe runtime path, oversized/partial frames, malformed Protobuf, missing capability | Reject safely, bound memory, preserve daemon and existing work; no automatic upgrade/restart | P1 |
| A25 | Restart desktop, companion, PC, and relay separately | Apply distinct recovery matrix; preserve slots through connection loss; revalidate/re-pair runtime terminal identifiers | P2, P3 |
| A26 | Resume same provider conversation; compact current conversation | Resume creates new run; pure compaction retains current run; old requests never migrate | P2, P3 |
| A27 | Silent shell exec/foreground transition with no PTY output or useful SIGCHLD | Periodic probe eventually detects incarnation/group change within documented budget; no activity inference | P2 |
| A28 | More runs than configured slots; pin, end, remove, and page changes | Stable slots and deterministic overflow. Explicit removal suppresses the exact run, and urgency changes preserve existing placements | P3 |
| A29 | Old/new schema peers; unknown optional fields, bodies, enum numbers; missing binding revision; differently encoded retries | Additive fields interoperate; reserved field/enum identifiers cannot be reused; unsupported semantics never become default actions/state; unknown state events invalidate cache; required presence is enforced; dedup uses semantic command identity | P1 foundation, P2 control methods, P3 presentation |
| A30 | Split/coalesced stream reads; bad preamble/kind/role; zero, overflowing, oversized, or truncated lengths; inconsistent render header; excessive nesting/collections | Exact framing survives arbitrary read boundaries; limits apply before allocation/mutation; invalid frames emit no partial ANSI; only offending connection fails and unrelated work continues | P1 |
| A31 | Regenerate schema on a clean pinned toolchain and round-trip uint64 above 2^53 and arbitrary bytes | Reproducible generated contract. IDs, revisions, and bytes round-trip exactly. Packaged binaries need no compiler | P1 build |
| A32 | Run equivalent tab/pane create, select, split, resize, close, detach, and reattach actions through TUI and GUI | Equivalent domain transitions, focus, terminal identity, and lifecycle outcomes. Unsupported capabilities are explicit | Later |
| A33 | Use the bundled Stream Deck daemon alongside the GUI; disconnect/reconnect hardware or the companion and change a slot during a press | Shared slot identities and exact focus; stale gestures are rejected without replay; hardware loss leaves terminal sessions running. Packaging, process lifecycle, and physical-device evidence remain open | Later |

## 16. Risks, remaining questions, and next work

### 16.1 Material risks and mitigations

| Risk | Consequence | Planned response / release gate |
|---|---|---|
| Incomplete provider causality or approval IDs | False input/working/completion claims | Per-field evidence, qualified capabilities, request sets, stale-event rejection, Unknown; A5/A9/A22 |
| Hook neutrality differs by version/event | Monitoring changes execution or blocks a CLI | Static neutral output, finite deadlines, no policy fields, fault-injection qualification; disable failing event; A17 |
| One PTY-owning daemon | Crash affects all workspaces; live processes not recoverable | Explicit matrix, metadata backup, no resurrection claim; supervisor deferred; A23 |
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
  [Backpressure measurements](02_daemon_foundation.md#backpressure-measurements)
  [Interactive key latency](02_daemon_foundation.md#interactive-key-latency)
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
