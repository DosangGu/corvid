# Provider Adapters and Exact Focus

> Split from the Corvid implementation design. See [Architecture Overview](overview.md) for context,
> [Providers Guide](../guides/providers.md) for hook installation, and
> [Provider Qualification](../qualification/providers.md) for captured traces.

## 6. Provider adapters and AgentMonitor

### 6.1 Adapter contract and qualification

Claude Code, Codex, and Antigravity CLI have separate adapters. Each adapter reports provider version,
execution mode (`interactive_cli`, `notify_only`, or a future `app_server`), adapter
schema revision, supported event kinds, ownership/correlation fields, and capabilities
such as `turn_start`, `response_boundary`, `explicit_question`, `permission_request`,
`request_resolution`, `subagent_identity`, and `revalidate_current_state`.

Capabilities are qualified (`exact`, `partial`, `unavailable`) with reasons; one
`supports_hooks` boolean is insufficient. Newer versions do not inherit new
capabilities from documentation alone. Registered providers have no minimum
version or exact-version allowlist on Linux and macOS; recognized releases use
their existing observer contract and validate its events and fields. Version admission is
separate from runtime qualification: Phase 2/3 fixtures record the exact installed
CLI release, and an accepted version is not a claim that its complete behavior
has been tested. The adapter registry supplies executable names, banner formats,
fixture baselines, and schema revisions. New agents add an explicit adapter that
normalizes observations into the shared collector and attention feed, without
provider-specific notification policy in Deck.
See [provider qualification](../qualification/providers.md) for captured traces
from Claude Code 2.1.318, Codex CLI 0.167.0, and Antigravity CLI 1.5.17. The agent projection includes the
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

#### Adding a provider integration

Register a provider in the domain provider enum, the Protobuf provider enum, and
the daemon's provider contracts. Keep its executable names, observer setup,
hook field requirements, evidence mapping, and qualified capabilities with the
registration. Add the provider's helper parser and CLI registration so it can
submit bounded collector messages. Map provider-specific observations to the
existing domain events where their semantics match. Preserve platform ownership
checks for the process, launch association, and hook producer.

Declare the saved-session ID syntax and build native resume arguments as
separate OS strings. Reuse the daemon's authenticated conversation observation
as confirmation when the provider can supply it. A provider without native
resume or exact confirmation returns a pane-level failure and leaves ordinary
shell recovery available. The daemon manages workspace reservation, deadlines,
cleanup, and run identities. A new run starts with its own request state.

Contract tests may register a synthetic provider and exercise correlation,
state storage, and recovery without a real CLI. Record real-provider runtime
qualification separately with the exact version, platform, and observed
behavior. Use the existing Claude and Codex fixtures to cover changes to their
adapter contracts.

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

Antigravity CLI (`agy`) evidence: [Hooks](https://antigravity.google/docs/hooks) and
[CLI reference](https://antigravity.google/docs/cli/reference), checked 2026-11-25, plus
local `agy 1.5.17` qualification. `agy` loads hooks only from `~/.gemini/config/hooks.json`,
`.agents/hooks.json`, or installed plugins (`--settings` does not accept `hooks`). `SessionStart`,
`PreInvocation`, `PostInvocation`, and `Stop` fail open with empty stdout and exit 0, whereas
`PreToolUse` in `1.5.17` fails closed when a hook is missing, crashes, or prints `{}`. Per
§6.4.6-7, Corvid registers only the four fail-open lifecycle events via explicit
`corvid hooks antigravity` (or `--install`), uses daemon-local turn generations because
`agy` hooks omit a shared turn ID, filters empty-`lastUserInput` subagent hooks, and relies
on `agy`'s native OSC 7501 `ProgramStatus` for passive permission/question wait visibility.

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
recognizes Claude's version banner and checks the launch association,
epoch, provider process incarnation, and the exec-form hook's parent PID.
`corvid observe --provider codex` uses the same collector. It also recognizes the
launcher's direct internal app-server child by its version-independent release
directory layout, installed executable, fixed local-server arguments, and process
incarnation. `corvid observe --provider antigravity` submits allowlisted observations
from `agy` (`conversationId`, `fullyIdle`, `terminationReason`, and non-empty `error`
indicator) without retaining `lastUserInput` or `transcriptPath`. A change in verified producer invalidates navigation for that run.
See [provider qualification](../qualification/providers.md) for the enabled scope.

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
7. An explicit managed launch requests observation for that session. Pass the
   adapter's observation hooks as additive session settings. Leave provider
   configuration files, trust, permissions, and execution policy under provider
   and user control. If neutrality cannot be established for a version/event,
   disable that observation capability.

Collector failures/gaps degrade state. A dropped hook cannot always report its own
failure; absent durable delivery, exact completeness is not promised. The architecture
chooses bounded observation over blocking the agent. Async hooks, when used, can also
arrive out of order and require the same correlation rules. Provider output behavior
is documented in the [Claude Hook guide](https://code.claude.com/docs/en/hooks-guide)
and [Codex Hook contract](https://learn.chatgpt.com/docs/hooks), checked 2026-11-07;
fault-injection qualification remains required.

A delivery gap for an authenticated launch permits collection to recover in the same
context. A new provider submission and matching confirmed execution or response
establish coverage for that prompt. Requests from the gap remain unverified until an
exact outcome resolves them. Ambiguous ownership and context transitions retain their
existing verification requirements. A planned daemon replacement preserves the
distinction between delivery and context gaps. Older handoff records with a gap
continue to require context verification.

### 6.5 Foreground detection is a different signal

AgentMonitor contains an observation collector and a process-identity probe. Foreground
identity can guide later keymap suggestions; it does not establish activity or completion.
OSC shell markers, user variables, process names, and output patterns are hints from
untrusted terminal content. They cannot create a trusted control command, resolve a
request, establish AgentSessionKey, or auto-remap keys in the initial release.

The terminal host retains bounded OSC 7501 program status records on each live PTY
and publishes the highest-priority report with the terminal state. It uses OSC 9;4
progress until the first valid OSC 7501 report and restores that fallback after a full
terminal reset. Reattach and retained-host recovery republish status silently. Desktop,
the composed TUI, and Agent Deck display these as application reports. Confirmed
provider requests take the summary position and retain their request IDs and alerts.
A new app report can schedule a rate-limited process identity check for a registered
agent with a known delivery gap. Collection recovery still requires provider evidence.
See [Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status)
and [#413](https://github.com/DosangGu/corvid/issues/413).

PTY output/SIGCHLD-only probing misses silent foreground changes, shell `exec`, resumed
jobs, grandchildren whose exit the daemon does not reap, and broken shell integration.
Use event-triggered probes plus a bounded periodic safety check: initially every 2 seconds
per tracked terminal, staggered, at most 20 probes/second host-wide. State a larger
worst-case detection delay when the terminal count consumes that budget. Probe process
incarnation and foreground group; debounce burst triggers. Failed probes produce Unknown,
not exit. Absence of output is never a completion timeout.

Corvid-created shells get a private PATH directory for registered provider
commands. A wrapper resolves the real program on the shell's current PATH and
adds session-scoped observers using the provider's contract. The daemon binds the launch
to the shell's live PTY using the peer PID, the foreground group, and the executable
image. It accepts observations only after the expected provider takes over that
process. Shell startup files and prompt hooks are left intact. Raw command lines
are not persisted.

Claude Code's native `/restart` can replace its executable in the same PTY.
Corvid accepts a new top-level startup or resume hook when the process keeps
its PID and birth identity, owns the foreground PTY, and runs the executable
currently selected by the original launcher path. The verified restart publishes
a fresh run for the conversation in the same pane. Requests from the previous
run stay in its history. When older handoff metadata lacks a launcher path,
Corvid can recover it from the invocation or PATH when the launcher still
identifies the verified executable before the restart.


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
   Do not acquire, release, or steal input rights.
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
See the [companion verification record](../qualification/platforms.md#desktop-and-companion-verification) for the
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
[Companion restart behavior](../guides/desktop.md#wezterm-on-windows)

A manual fallback lets the user confirm an existing terminal and explicit ClientId,
then performs remote focus while returning ManualRequired locally. This supports the
Phase 2 virtual controller without claiming automatic terminal discovery. With no
attached/registered client, display ConnectionRequired/NoAttachedClient. For another
host, require an existing registration to that host; never reuse a convenient client on
the current host. Paired bindings expire on client disconnect, explicit unregister, or
epoch change. Control-channel loss alone does not destroy a still-live TUI binding.
