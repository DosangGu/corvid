# Coding and Testing Guidelines

## Coding Style and Naming Conventions

Write documentation in English and distinguish proposals from implemented behavior.
For Rust, use rustfmt defaults, four-space indentation, `snake_case` functions/modules,
`UpperCamelCase` types, and `SCREAMING_SNAKE_CASE` constants.

- Keep domain types independent of generated Protobuf messages.
- Never hand-edit generated code or reuse removed Protobuf field numbers; reserve them instead.
- Put statements on separate lines; separate logical stages with blank lines.
- Wrap long signatures, calls, chains, and conditions using rustfmt. Name intermediate
  values to clarify complex expressions.
- Give each function one coherent responsibility. Extract descriptive helpers for
  distinct stages, repeated logic, or complex branches; keep orchestration readable.
- Prefer early returns over unnecessary nesting. Split by responsibility, not arbitrary
  line counts; avoid helpers that merely rename trivial expressions.
- Review readability beyond formatter checks.

## Testing Guidelines

Keep Rust unit tests beside modules and integration tests under each crate's `tests/`.
Name tests after behavior, e.g. `rejects_stale_epoch`. Follow the design's acceptance
matrix below: identity, reconnection, input fencing, and malformed frames. No coverage
threshold is configured. Distinguish executed checks from planned tests and unverified
runtime compatibility.

## Acceptance Matrix

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
| A6 | Response boundary, then agent process exit with the owning pane still open | Response retains the card; confirmed exit moves it to history and compacts following cards from the FIFO queue | P2, P3 |
| A7 | PC/control link off while remote agent asks a question | Healthy remote collector retains request; full resync displays it without replaying a prompt/answer | P2, P3 |
| A8 | Mutate request and move pane during snapshot encoding | Immutable S plus contiguous deltas yields latest consistent result; gap/overflow triggers full resync | P2 |
| A9 | Duplicate, delayed, reversed events; old Stop after new turn; `/clear`; child shares environment | No old/child event overwrites new/parent activity; bounded orphan correlation; ambiguity explicitly visible | P2, P3 |
| A10 | No registration/client, stale local window, or ambiguous registered clients | Exact error/ConnectionRequired; no terminal/tab/interactive SSH/agent launch or substitute client | P2, P3 |
| A11 | Remote reboot, then send old focus command | EpochMismatch; old IDs not interpreted, old Live state not resurrected | P1, P2 |
| A12 | Reuse PID and pane display number for a different agent | Old run or slot capture cannot target the replacement. Process incarnation check prevents reuse confusion | P2 |
| A13 | High PTY output plus stalled state watcher/render client | Bounded queues/memory; watcher resync or render disconnect; ordered parser bytes and unrelated agents continue within host resource budget | P1, P2 |
| A14 | Attach clients with different sizes to one workspace | Independent pane sizes; resuming an occupied pane fences its previous writer and applies the new viewport; no writer keeps last size | P1, Later |
| A15 | Updated CLI meets an old running daemon | Compatible negotiation or clear ProtocolMismatch; daemon/processes stay alive | P1 |
| A16 | Slot reassigned or page changed during press | Captured run/revision rejected as stale, never redirected to new occupant; no replay after reconnect | P2, P3 |
| A17 | Corrupt/newer-schema storage, unsupported events, hook timeout/crash/missing helper/full queue | Preserve/recover files safely; no guessed Live state, policy change, blocked agent, or unlimited diagnostics | P1–P3 |
| A18 | Agent emits forged OSC/title/question text containing shell/control syntax | Display sanitization; no shell interpolation, focus, approval, or trusted lifecycle mutation | P2 |
| A19 | Fragmented UTF-8, Korean/wide/combining, alternate screen/cursor/color/erase/scroll, resize, paste, Ctrl-C/Esc/Alt, queries, TERM profile | Qualified behavior matches advertised capabilities; full redraw reconstructs current visible state | P0, P1 |
| A20 | Focus/resize/disconnect while keys or paste are buffered; input ownership transfer | Fenced old tokens rejected; accepted paste stays at original target; partial delivery not retried; no stale-key replay | P1, Later |
| A21 | Long-running child; abrupt SSH disconnect and clean last logout | Same daemon/PTY/process survives on documented deployment; reattach observes continued progress | P1 |
| A22 | Collector gap followed by full control resync | Cache consistent but affected activity/request remains Unknown/Unverified until actual evidence | P2 |
| A23 | Kill PTY-owning daemon mid-work, restart with saved layout | New epoch and empty PTY slots until a client opens a pane. A verified saved conversation resumes as a new run and old state stays historical | P1 |
| A24 | Unauthorized UID, unsafe runtime path, oversized/partial frames, malformed Protobuf, missing capability | Reject safely, bound memory, preserve daemon and existing work; no automatic upgrade/restart | P1 |
| A25 | Restart desktop, companion, PC, and relay separately | Apply distinct recovery matrix; preserve slots through connection loss; revalidate/re-pair runtime terminal identifiers. Desktop restores open windows independently without taking over occupied panes; explicitly closed windows stay closed | P2, P3 |
| A26 | Resume same provider conversation; compact current conversation | Resume creates new run; pure compaction retains current run; old requests never migrate | P2, P3 |
| A27 | Silent shell exec/foreground transition with no PTY output or useful SIGCHLD | Periodic probe eventually detects incarnation/group change within documented budget; no activity inference | P2 |
| A28 | More runs than slots; completion, pin, pane closure, and page changes | Completed runs stay in place or queue. Pane closure packs remaining cards and the FIFO queue forward; pins follow their agents and old revisions cannot retarget | P3 |
| A29 | Old/new schema peers; unknown optional fields, bodies, enum numbers; missing binding revision; differently encoded retries | Additive fields interoperate; reserved field/enum identifiers cannot be reused; unsupported semantics never become default actions/state; unknown state events invalidate cache; required presence is enforced; dedup uses semantic command identity | P1 foundation, P2 control methods, P3 presentation |
| A30 | Split/coalesced stream reads; bad preamble/kind/role; zero, overflowing, oversized, or truncated lengths; inconsistent render header; excessive nesting/collections | Exact framing survives arbitrary read boundaries; limits apply before allocation/mutation; invalid frames emit no partial ANSI; only offending connection fails and unrelated work continues | P1 |
| A31 | Regenerate schema on a clean pinned toolchain and round-trip uint64 above 2^53 and arbitrary bytes | Reproducible generated contract. IDs, revisions, and bytes round-trip exactly. Packaged binaries need no compiler | P1 build |
| A32 | Run equivalent tab/pane create, select, split, resize, close, detach, and reattach actions through TUI and GUI | Equivalent domain transitions, focus, terminal identity, and lifecycle outcomes. Unsupported capabilities are explicit | Later |
