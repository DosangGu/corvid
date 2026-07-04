# corvid — AI Agent Multiplexer: Master Design Document

> Rust · TUI · client-server split · inspired by zellij / zellij-attention
> **Identity: not a general-purpose terminal, but an "AI coding-agent orchestration multiplexer"**

This single document holds the whole design: locked decisions + roadmap + detailed components.

---

## Table of Contents
1. What we're building
2. Mental model (terminology)
3. Locked core decisions (summary)
4. Architecture: client-server
5. IPC protocol
6. Server internals
7. Killer feature ①: attention monitoring
8. Killer feature ②: automatic key remapping
9. AgentMonitor detection mechanism (detailed)
10. Connection lifecycle
11. Stack
12. Roadmap (Phase 0–5)
13. Risks / open questions
14. Next work

---

## 1. What we're building

A zellij-style TUI multiplexer that lets you **monitor at a glance** the AI coding agents
(claude code, codex, etc.) running across multiple workspaces/tabs/panes (who finished, who's
waiting for input) and **control each agent without keybinding conflicts**.

Directions discarded during design evolution:
- GUI-native emulator → **TUI** (runs on top of an existing terminal)
- Single process → **client-server split** (resume by attaching from another machine over SSH)
- General-purpose emulator → **agent-orchestration specialized**

---

## 2. Mental model (terminology)

```
Workspace            ← zellij's "session" (top level, unit of detach/resume)
  └─ Tab             ← tab (tab bar can be placed vertically on left/right)
       └─ Pane       ← split region (arbitrary-depth N-way tiling tree)
            └─ Terminal (one PTY)   ← one AI agent runs here
```

- The "session = one PTY" definition stays, but the name is `Terminal`. The word `session`
  collides too much with tmux/SSH/zellij, so it isn't used at the PTY level. zellij's session is
  promoted to `Workspace`.
- Tab bar: zellij puts it horizontally on top. We add a **vertical placement option (left/right)**
  (doable within the TUI cell grid; Edge-browser-level pixel customization is unnecessary).
- Panes use **N-way tiling** (arbitrary-depth split tree, zellij/tmux style). The "diff next to
  agent" use case demands splitting, and to avoid the migration cost of going from an intermediate
  model to a tree, we go with a tree from the start.

---

## 3. Locked core decisions (summary)

| Decision | Value | Reason |
|---|---|---|
| Form factor | TUI | runs atop existing terminal, SSH-friendly |
| Layout | N-way tiling tree | general splitting, diff+agent+logs at once |
| Process model | client-server split | core of detach/resume |
| **IPC scope** | **always local, within one machine** | ssh is just the door into the machine; IPC is a Unix socket inside H |
| Transport | Unix socket (Windows named pipe) | local, so auth = socket permissions, done |
| Serialization | **bincode + serde** | client & server are always the same binary, no version mismatch |
| **Keymap resolution location** | **server** | auto-remapping depends on info only the server has (foreground) |
| Key input | client sends only raw keys | resolution is entirely the server's job |
| Render location | **server** (ratatui) | server generates per-client ANSI; client is stateless |
| Render optimization | delegated to ratatui cell diff | not a design decision; optimize after measuring |
| Keymap trigger | **based on the single focused pane** | even with N visible, only 1 (focus) receives input |
| Detection method | **event-first + polling fallback** | OSC/hook first, /proc as fallback |

---

## 4. Architecture: client-server

Borrowed from the zellij model. This is the core of "resume by attaching from another machine over SSH."

```
┌─────────────┐        Unix socket         ┌───────────────────────────┐
│   Client    │  ───── input (raw key) ──▶ │          Server            │
│ (each term) │  ◀──── render ANSI ─────── │ (owns PTY/layout/state)    │
│  ephemeral  │                            │  daemonized, persistent    │
└─────────────┘                            └───────────────────────────┘
```

### ★ Premise: IPC is always "local, within one machine." ssh is just the door into the machine.
```
[local laptop] ──ssh──▶ [machine H]
                        └─ from H's shell: `corvid attach backend-code`
                        └─ both client and server run entirely inside H
                        └─ IPC is a Unix socket inside H. ssh isn't above it; it's H's door.
```
→ From the multiplexer's point of view it's **always local**. It never has to think about the network.
→ Client and server are **always the same installation on the same machine** = binary versions always match.

**What we are NOT doing**: "network IPC (TCP/TLS) remote attach" with a local client and a remote
server (the zellij 0.44 HTTPS model). That pulls auth, TLS, reconnection, and bandwidth into the
IPC layer, which is far heavier. **Out of scope.**

### Roles
- **Server**: a daemon. The first client launches and daemonizes it. It survives even after the
  last client leaves (ActiveDetached, PTYs retained).
- **Full client**: holds the terminal, raw mode, captures input + outputs renders. Stateless.
- **CLI client**: one-shot. Injects an attention signal/action and exits (`corvid pipe`, `corvid action`).

### Sockets & discovery
```
$XDG_RUNTIME_DIR/corvid/<workspace_name>.sock         # one socket per workspace
$XDG_STATE_HOME/corvid/<workspace_name>/layout.json   # serialized for resurrection
```
- Workspace name = socket name. `corvid attach backend-code`.
- Live socket = Active; only layout.json present with a dead socket = Resurrectable.
- Liveness check: `ConnStatus` handshake after connecting (zellij `assert_socket` pattern).

---

## 5. IPC protocol

### Framing
- Length prefix (u32 LE) + `bincode` payload.
- Two logical channels over one connection (initially distinguished by message type, split into
  separate sockets later if needed):
  - input channel: client→server (keys/mouse/resize)
  - render channel: server→client (render bytes/control)

### Client → Server
```rust
enum ClientToServer {
    // connection setup
    Attach {
        client_id_hint: Option<ClientId>,   // on reconnect
        attrs: ClientAttributes,            // terminal size, color/unicode support, $TERM
        workspace: String,
    },
    Detach,
    ConnStatus,                              // liveness handshake

    // input (★ raw, not resolved — keymap is the server's job)
    KeyEvent(RawKeyEvent),
    MouseEvent(RawMouseEvent),
    Paste(Vec<u8>),
    TerminalResize(Size),

    // actions (CLI client or explicit macros)
    Action(Action),                          // split/move/new-tab, etc.
    Pipe(PipeMessage),                       // attention::needs_input::<pane_id>, etc.
}
```

### Server → Client
```rust
enum ServerToClient {
    Connected { client_id: ClientId },

    // render (server generates per-client ANSI; client writes it to stdout as-is)
    Render { bytes: Vec<u8> },

    // state pushes (for display)
    ActiveKeymapChanged {                    // ★ notification of auto-remap result
        pane_id: PaneId,
        profile: String,                     // "claude-code" / "codex" / "default"
        hints: Vec<KeyHint>,                 // for hint bar / toast
    },
    AttentionUpdate {                        // global notification bar update
        items: Vec<AttentionItem>,           // {workspace, tab, pane, title, state}
    },
    SessionUpdate(SessionInfo),
    Log(String),
    Exit(ExitReason),
}
```

Key point: rendering is **generated per-client by the server** (clients have different terminal
sizes). Keymap/attention are pushed as separate state messages → the client only displays. The
client must hold zero state so that attaching from another machine restores everything.

---

## 6. Server internals (zellij Bus pattern)

```
Router thread        ── receive ClientToServer from socket → translate/dispatch to internal instructions
  ├─▶ Screen thread   ── pane/tab/workspace state, layout-tree traversal, render trigger
  │     └─ ratatui render → per-client ANSI generation → Render message
  ├─▶ Pty thread(s)   ── PTY spawn/read/write, VTE-parse to update each pane's grid
  ├─▶ AgentMonitor    ── ★ event collector + conditional fallback poller (detailed in §9)
  │     └─ OSC133/user-var/CLI events first; /proc polling only when no shell integration
  │     └─ focus pane identity change → switch keymap profile → notify Screen
  └─▶ Persistence     ── layout serialization, resurrection
```
- Instructions passed between threads via channels (`crossbeam`/`tokio::mpsc`).
- **AgentMonitor is our extra thread** — zellij doesn't have it.

---

## 7. Killer feature ①: attention monitoring

Elevates the zellij-attention idea to a first-class core feature.

### State model (per pane)
- `Working` — working (default)
- `NeedsInput` — needs user interaction (⏳, specific color)
- `Completed` — task done (✅, specific color)
- `Idle` — waiting at shell prompt

### Display
- **Icon+color on tab/pane title**: auto-cleared on focus (memory-only state, exactly like
  zellij-attention).
- **Global notification bar** (bottom/top): one line like "⏳ workspace2/tab3 'claude-refactor'
  needs input" telling you **which workspace's which titled terminal needs attention**.
- **Vertical tab bar** also shows a status color dot next to each tab.
- External notification (optional): push via ntfy.sh, etc. (for multi-machine / away-from-desk).

Signal reception paths are in §9 (AgentMonitor).

---

## 8. Killer feature ②: automatic key remapping

"While claude code runs, claude's shortcuts and the multiplexer's keys don't collide; while codex
runs, codex's keys and ours don't collide — auto-remapped. Even if keys change, they're displayed,
so it's safe."

### ★ Key simplification: even as N grows, there's always exactly 1 "active keymap"
Even with N agents visible on screen, **only the focused 1 receives key input**.
→ Auto-remapping always looks at just **the foreground process of the focused pane**, regardless of N.
→ Monitoring (attention) covers all N; remapping covers 1. The two concerns are separated.

### Two trigger paths
```
[keymap re-evaluation] — recompute the focus pane's active profile
  • focus move (pane B → pane A)
  • foreground change in the focused pane (e.g., bash → claude)
      → switch profile → default keymap ∩ reserved-key conflict → move colliding corvid key to an alternate
      → push ActiveKeymapChanged to all attached clients
      → client: update hint bar + "claude code detected → split=Ctrl+Alt+S" toast

[attention only] — keymap untouched
  • foreground/state change in non-focused panes → push AttentionUpdate only
```

### Input flow (focus-based, so contention is eliminated at the source)
- Client sends `KeyEvent(Ctrl+S)` raw
- Server resolves it with **the focus pane's active profile**:
  - if a corvid control key → execute Action
  - otherwise → write straight to the focus pane's PTY (passthrough)
- Input going to a non-focused pane is **guaranteed not to exist** → no passthrough/remap contention.
- **Modal lock mode** (orthogonal to profiles): when unlocked, almost all keys passthrough; when
  locked, the corvid keymap is active. Remapping further reduces conflicts even in lock mode.
  **Make modal the default safety net, auto-remapping an advanced option.**
- Local key latency: same-machine Unix socket means microsecond RTT → non-issue.

### ★ UX nail: focus indication and keymap indication are a pair
With N-way tiling, if "which pane is focused" is unclear, the active keymap gets confusing.
→ **Focus pane visual indication (border/color) + hint bar profile name ("claude-code") always as a pair.**
These two together are what makes it trustworthy.

---

## 9. AgentMonitor detection mechanism (detailed)

Verified references: wmux / wtmux / psmux (all already implement "multiple AI agents in a
multiplexer + auto-detection").

### 9-1. One component, two signals of different character
| | What | Consumer | Frequency | Change moment |
|---|---|---|---|---|
| **A. foreground identity** | bash/claude/codex | auto-remap | rare | entering/exiting shell↔agent |
| **B. agent state** | working/needs-input/done | attention | frequent | continuously while working |

Don't bind the two into one polling cycle. A is event-driven; B is signaled directly by the agent.

### 9-2. Three-tier strategy (by reliability)
```
Tier 1: explicit signal (most accurate, event)
  • agent hook → `corvid pipe` CLI    → directly reports B (state)
  • intercept OSC 133 shell-integration markers → event for A (command start/end)
  • OSC user-var (WEZTERM_PROG style) → directly reports A (command-line identity)
Tier 2: /proc polling (auxiliary, when no signal)
  • tpgid → comm/cmdline             → infer A
Tier 3: output heuristics (last resort)
  • PTY output patterns / idle detection → guess B (low reliability)
```
Principle: **event-first + polling safety net.** Not either/or.

### 9-3. A (identity) — why it becomes an event
- The kernel has no "foreground change" event. tpgid is a state value readable only by polling
  (`/proc/<pid>/stat`'s tpgid field, or `tcgetpgrp()` on the PTY master).
- But the change only happens when the shell fork+execs a child (entering claude) or that child
  dies. Signals to catch at those moments:
  - **OSC 133** ★: the shell emits `133;C` just before running a command, `133;A/B/D` on returning
    to the prompt. Since we already parse PTY output, intercepting these markers gives a precise
    "command start/end" event. Re-evaluate A only on receiving C.
  - **OSC user-var** ★: shell integration emits the running command line → we receive "claude/codex"
    without guessing. Sidesteps the problem of `comm` hiding behind a wrapper (node/python).
- **/proc fallback** (only when no shell integration): tpgid → comm + cmdline if needed. **Triggered
  polling** — check only when there's PTY output, or right after SIGCHLD; idle panes aren't polled.
  macOS: no /proc → `libproc`/`sysctl(KERN_PROC)`; tcgetpgrp is the same.

### 9-4. B (state) — the agent signals directly
- **Tier 1: hook → CLI** (exactly like zellij-attention):
  - claude `Notification` hook → `corvid pipe attention::needs_input::$PANE_ID`
  - claude `Stop` hook → `corvid pipe attention::completed::$PANE_ID`
  - codex etc. hit the same CLI via their own hooks. A unified interface regardless of agent.
- **Tier 1 alt: custom OSC** — if the agent emits one, we receive it directly in PTY parsing;
  propagates over SSH automatically.
- **Tier 3 fallback: output heuristics** — guesswork for agents that don't signal; auxiliary display only.

### 9-5. AgentMonitor = event collector + conditional fallback poller
```
AgentMonitor
  ├─ [event] OSC 133 / user-var / custom OSC intercepted by the VTE parser → update A·B
  ├─ [event] attention signal arriving via the corvid pipe CLI → update B
  └─ [conditional poller] only panes without shell integration, check tpgid on PTY output/SIGCHLD → fallback for A
```
→ Near-zero CPU at rest (event-driven). Polling only on shell-integration-less panes, and only when there's activity.

### 9-6. Shell integration injection
OSC 133/user-var require the shell to emit them. Two paths:
- The user sources a shell-integration script (.bashrc/.zshrc) — explicit, safe.
- corvid auto-injects into the shell it spawns (PROMPT_COMMAND/precmd) — convenient but watch for
  collisions with user config; opt-in.
→ With integration: accurate, event-driven. Without: graceful degrade to /proc fallback. **Both work.**

---

## 10. Connection lifecycle

### 10-1. New attach (local)
```
client: no socket → spawn + daemonize server → socket appears
client → Attach{workspace, attrs}
server → Connected{client_id} → Render{initial screen}
[loop] client → KeyEvent/Resize ;  server → Render/state push
```

### 10-2. Resume by attaching from another machine over SSH ★ core scenario
```
(server is alive as ActiveDetached on machine H, PTYs intact)
on machine M: ssh H → from H's shell, `corvid attach backend-code`
client (inside H's shell) → Attach{workspace:"backend-code", attrs: M's terminal size}
server → Connected + Render{current state as-is}
→ re-render per-client to M's terminal size. The agents' work is uninterrupted.
```
- detach (client exits / SSH drops): server cleans up the client_id, retains PTY/state.
- concurrent multi-attach: H-local and M-remote can both view the same workspace at their own sizes.

### 10-3. Resurrection (server died and comes back)
```
server died (reboot, etc.) → no socket, only layout.json remains
client → `corvid attach` → spawn server → load layout.json
→ restore pane structure/CWD/scrollback. But the processes themselves are NOT re-run.
```

### 10-4. Agent-process resume policy (open)
Resurrection restores only the layout; the claude/codex processes that were running don't survive
(same limitation as zellij).
- (a) empty shell + match CWD only (safe, **default**)
- (b) store a per-pane "resume command" as metadata → auto-run (risky, only on explicit config)
- (c) agent-native resume (`claude --resume <id>`, etc.) → opt-in when the agent supports it

---

## 11. Stack

| Layer | Crate | Notes |
|---|---|---|
| TUI render | `ratatui` | de-facto standard. cell diff built in |
| Terminal backend | `crossterm` | cross-platform (incl. Windows) |
| PTY spawn | `portable-pty` | Unix PTY + Windows ConPTY (⚠️ below) |
| Pane content parsing | `wezterm-term` or `vt100` | PTY output → cell grid (scrollback/SGR/cursor) |
| Serialization | `serde` + `bincode` | layout persistence + IPC |
| IPC / runtime | `tokio` + Unix socket | client↔server, PTY I/O multiplexing |
| Config | `serde` + KDL/TOML | keybindings, colors |

**Reference implementations**: zellij source (client/server split, `session_serialization.rs`),
wmux/wtmux/psmux (agent auto-detection).

**⚠️ Windows ConPTY**: upstream portable-pty doesn't pass the latest ConPTY flags
(RESIZE_QUIRK / WIN32_INPUT_MODE / PASSTHROUGH_MODE). See the `portable-pty-psmux` patched fork.
**Strategy: Linux/macOS first with the same code; Windows later, branching only the PTY layer via cfg.**

---

## 12. Roadmap (Phase 0–5)

### Phase 0 — single-process TUI MVP
- [ ] boot `ratatui`+`crossterm`, spawn one shell via `portable-pty`
- [ ] PTY output → grid parse (`wezterm-term`/`vt100`) → ratatui render
- [ ] input passthrough, resize handling
- ✅ a single-pane TUI terminal (no server split yet)

### Phase 1 — Pane/Tab/Workspace
- [ ] N-way split (tree), tabs, workspaces + focus/move
- [ ] **vertical tab bar** render (left/right option)
- [ ] layout serialization (save)
- ✅ zellij-style multiplexing

### Phase 2 — client-server split ★ biggest job (the backbone)
- [ ] server daemonization, Unix socket IPC, per-client render
- [ ] detach/attach, ActiveDetached state
- [ ] verify attach-from-another-machine-over-SSH → resume
- [ ] resurrection (restore layout on server restart)
- ✅ "resume even when attaching from another machine" achieved

### Phase 3 — modal keybindings + hint bar
- [ ] Mode enum, per-mode keymaps, bottom hint bar
- [ ] KDL/TOML config loading
- ✅ foundation for a discoverable UX (prerequisite for §8)

### Phase 4 — attention monitoring ★ killer ①
- [ ] `corvid pipe` CLI + hook reception, state model
- [ ] OSC interception path
- [ ] tab/pane icon+color, global notification bar
- [ ] document claude code hook integration example, (optional) ntfy.sh push
- ✅ monitor multiple agents at a glance

### Phase 5 — automatic key remapping ★ killer ②
- [ ] foreground detection: OSC 133/user-var events + /proc fallback
- [ ] agent keymap profiles + conflict detection/auto-move
- [ ] modal lock mode in parallel
- [ ] keymap-switch indication (hint bar + toast)
- ✅ conflict-free agent control

### Later
- Windows (ConPTY process detection/flags)
- remote TCP/TLS attach (beyond the SSH tunnel)
- agent-session auto-resume (on resurrection)
- scrollback search, mouse reporting, IME

---

## 13. Risks / open questions

### Risks
1. **Pane ANSI parsing performance**: N agents dumping output at once → grid parse/compose cost.
   Update only dirty regions; one parser instance per pane.
2. **Foreground detection limits → mostly resolved by OSC user-var**: the comm-hidden-behind-wrapper
   problem is sidestepped by shell-integration user-vars. Fall back to cmdline matching + patterns
   only when absent. **Signal first, polling as fallback.**
3. **Client-server is 80% of the difficulty**: Phase 2 is the project's backbone. The IPC protocol /
   per-client render / state synchronization design governs everything.
4. **Remapping vs modal priority**: make modal lock the default safety net, auto-remapping the advanced option.
5. **Agent definition data**: claude/codex reserved keys and hook formats change per version. Keep them as external data.

### Open questions (decide while measuring in each Phase)
- AgentMonitor fallback-polling debounce interval (prevent over-reading tpgid during output bursts).
- cmdline matching ruleset for user-var-less shells (claude/codex/aider/gemini... — wmux detects 7 kinds).
- default for shell-integration auto-injection on/off.
- default/opt-in scope of the agent-process resume policy (§10-4).

### Closed (resolved by the local-IPC premise)
- Serialization → bincode confirmed (no version mismatch).
- Local key latency → non-issue (same-machine RTT in microseconds).
- CLI client auth → socket file permissions, done.
- Render cell diff → delegated to ratatui.
