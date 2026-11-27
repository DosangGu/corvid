# Provider Qualification

> Captured traces, event mappings, and reproduction scripts per provider release.
> For hook installation and adapter overview, see [Provider Hooks and Observers](../guides/providers.md).

## Working activity qualification

On Linux, interactive Codex CLI 0.171.0 and Claude Code 2.1.329 ran through
Corvid PTYs against local synthetic model servers on 2026-11-23. The Codex
observer received no submission for the test turn. `PreModelRequest` still
published **Working** while the response was held, and `PostModelRequest`
reported the attempt before `Stop` published **Response observed**. Claude's
root `Read` hooks shared a session, prompt, and tool ID. The tool result
published **Working** while the next model response was held. The Claude
observer also registers `Bash`; the live case exercised `Read`.

Reducer fixtures cover retry attempts, responses after a continued Stop,
interruption, delayed activity from earlier turns, and collection gaps. The
native X11 fixture renders mixed activity and input evidence and checks focus
routing. Windows ConPTY fixtures exercise the generated hook commands. The
interactive releases above were exercised on Linux.

## Claude Code 2.1.318 on Linux

Recorded on Linux 7.2.14 on 2026-11-07 and 2026-11-08. The fixture launches the installed
interactive CLI in a real PTY with screen-reader output. It uses a temporary
home, an isolated workspace, and a loopback Messages server that supplies synthetic
responses. Question scenarios use manual permission mode. The plan scenario
starts in plan mode. The fixture approves the plan or returns feedback in the PTY.

| Event | Observed fields and behavior |
|---|---|
| `SessionStart` | Startup supplies a session ID and `source=startup`. The first prompt ID is absent. |
| `UserPromptSubmit` | Supplies a prompt ID under the current session. |
| `PreToolUse` and `PostToolUse` | The observed `AskUserQuestion` invocation and result share session, prompt, and tool-use IDs. |
| `ExitPlanMode` | After writing a draft and loading the deferred tool, the CLI shows an approval dialog. Its `PreToolUse` and successful `PostToolUse` share session, prompt, and tool-use IDs. |
| Plan rejection | The manual rejection reports the exact `ExitPlanMode` tool-use ID in `PostToolBatch` before `Stop` for the same prompt. The batch entry carries no typed outcome. The fixture receives no matching success, failure, or denial hook. |
| `PermissionRequest` | The same question also emits this event with the session, prompt, and tool name. The tool-use ID is absent. Require separate correlation evidence before resolving a permission observation. |
| Question dismissal | Esc closes the question. The next submission and `Stop` carry a new prompt ID. The dismissed question has no matching result, failure, or denial hook in this trace. |
| `Stop` | The response retains the prompt ID after the tool result. Other hooks can continue the turn, so this boundary provides response evidence. |
| Background `Agent` | `SubagentStart` and `SubagentStop` report the parent's session and prompt IDs with a distinct `agent_id`. The parent's `PostToolUse` result reports that ID as `tool_response.agentId` with `status=async_launched`. |
| Foreground `Agent` | The child's `SubagentStop` precedes the parent's `PostToolUse`. The result carries `status=completed` and the child's exact `agentId` under the same session and prompt. |
| Nested `Agent` | The result hook's `agent_id` names the invoking actor, and `tool_response.agentId` names its child. The descendant can finish before the invoking actor is linked to the top-level run. |
| `SessionEnd` and `SessionStart` | `/clear` ends the old session with `reason=clear` and starts a distinct session with `source=clear`. The CLI stays in the same PTY. `/exit` later ends the new context with `reason=prompt_input_exit`. |
| `/compact` and `/resume` | Manual compaction emits `SessionStart` with `source=compact` and the same session ID. Switching back to a previous conversation emits `SessionEnd` with `reason=resume` for the outgoing session, followed by `SessionStart` with `source=resume` for the target. Both sides report the same prompt ID. |

Use the [Claude hook reference](https://code.claude.com/docs/en/hooks) for the
documented payloads and the [Messages streaming reference](https://platform.claude.com/docs/en/build-with-claude/streaming)
for the fixture server's response format.

## Reproduce

```sh
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid-tui/tests/claude_qualification.py --scenario answer
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid-tui/tests/claude_qualification.py --scenario clear
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid-tui/tests/claude_qualification.py --scenario resume
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid-tui/tests/claude_qualification.py --scenario cancel
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid-tui/tests/claude_qualification.py --scenario plan
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid-tui/tests/claude_qualification.py --scenario plan-rejected
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid-tui/tests/claude_qualification.py --scenario child
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid-tui/tests/claude_qualification.py --scenario child-sync
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid-tui/tests/claude_qualification.py --scenario child-nested
```

The [fixtures](../../crates/corvid-tui/tests/fixtures/claude/2.1.318/) keep allowlisted
fields and replace captured IDs with deterministic placeholders. The capture
hook exits successfully with empty stdout.


## Codex CLI 0.167.0 on Linux

Recorded on Linux 7.2.14 with the standalone x86_64 musl release. The interactive
CLI ran in a real PTY with a temporary home, a synthetic loopback Responses
provider, and the CLI's interactive hook trust screen. The test used tmux 3.8 to
inspect the outer terminal. Corvid does not need tmux to run the adapter.

| Event | Observed fields and Corvid behavior |
|---|---|
| `SessionStart` | Initial `source=startup` identifies a session. `/new` also emits `startup` for a distinct session. Corvid quarantines that boundary and disables focus for the old run until the operator starts a newly qualified managed context. |
| `UserPromptSubmit` | Session and turn IDs are present. Submission is tentative because other hooks can still block it. |
| Bash `PreToolUse` / `PostToolUse` | An approved synthetic command carries the same session, turn, and tool-use IDs. Corvid tracks and closes that exact tool scope. |
| Bash `PermissionRequest` | Reports the turn with no tool-use ID. The real approval dialog remains visible. The separate permission observation stays Unverified after the command runs. |
| `request_user_input` | Plan mode displays the question and emits a matching `PostToolUse` after the user answers. Default mode can emit `PreToolUse` without a dialog. Corvid records a typed Unverified question and resolves it only on the matching result. |
| `Interrupt` | Canceling a Plan question reports its exact turn. Corvid cancels that turn's correlated open scopes and rejects late attempts to reopen them. Other runs and uncorrelated permission observations retain their state. |
| `Stop` | Reports a turn and `stop_hook_active`. Corvid records the response boundary and emits one response alert per turn. A later model request can confirm renewed activity on the same turn. The observer emits a fixed empty JSON object. |
| `/compact` | The manual `PreCompact` and `PostCompact` hooks share the current session and turn IDs. The next prompt emits `SessionStart` with `source=compact` for that session. Corvid retains the run and its requests. |
| `resume` | The first prompt after resuming emits `SessionStart` with `source=resume` for the same conversation ID. A fresh managed process gets a new run and an empty request history. |
| `notify` | A root completion can match the hook session and turn. The same executable also delivers completions with other thread IDs that have no matching lifecycle hook in this fixture. Corvid does not use `notify` to establish an interactive run's identity. |

The in-process `/resume` can switch the view before its first hook. Its
`SessionEnd` reports `reason=other` without a transition ID. Corvid marks the
observed in-process boundary uncertain and suspends focus for the old run. It
uses OS exit evidence for process liveness. Plan approval and child event
ownership remain outside this release qualification.
[Codex Hook contract](https://learn.chatgpt.com/docs/hooks)
[Codex notification payload](https://learn.chatgpt.com/docs/config-file/config-advanced#notifications)


### Reproduce

```sh
cargo build --locked -p corvid-tui
export PYTHONDONTWRITEBYTECODE=1
python3 crates/corvid-tui/tests/codex_qualification.py --corvid target/debug/corvid
python3 crates/corvid-tui/tests/codex_qualification.py --corvid target/debug/corvid --conversation
python3 crates/corvid-tui/tests/codex_qualification.py --corvid target/debug/corvid --parallel
python3 crates/corvid-tui/tests/codex_qualification.py --corvid target/debug/corvid --enable-auto-start
for fault in missing-helper missing-collector timeout collector-saturated; do
  python3 crates/corvid-tui/tests/codex_qualification.py --corvid target/debug/corvid --fault "$fault"
done
for fault in missing-helper missing-collector; do
  python3 crates/corvid-tui/tests/codex_qualification.py --corvid target/debug/corvid --conversation --fault "$fault"
done
for mode in disable enable; do
  python3 crates/corvid-tui/tests/codex_qualification.py --corvid target/debug/corvid --conversation "--$mode-auto-start" --fault helper-crash
done
```

The [allowlisted traces](../../crates/corvid-tui/tests/fixtures/codex/0.167.0/) use
fixture identifiers. For A5 and A7, the parallel fixture uses loopback OpenSSH
10.6p1 and temporary keys. It kills only the companion's SSH process and
holds the relay offline while two same-directory Codex runs display questions
with the same tool-use ID. Reconnection keeps the companion and daemon epochs
and the two slot placements. It recovers both Unverified question records
without adding hook events. The companion focuses the exact registered TUI
and acknowledges its alert while the question stays visible. Answering that
question resolves only its RequestId. Interrupting the other TUI cancels the
other RequestId.

For A25 recovery case E, the same fixture also kills the remote
`corvid control --stdio` watch process after pairing the first TUI and parking
its question on another tab. The offline view retains both slots. A new relay
restores the full snapshot with both agent PIDs, TUI client identities, and the
binding revision intact. The old binding capture is refused, and a fresh one
focuses the original question without answering it.

The failure tests exercised a real approval and completed its response
despite the missing or timed-out observer, missing socket, and 32
occupied collector connections. The saturation test also checked that the next
connection was refused. The conversation fixture completed
compaction and resume with either the helper or the collector socket absent.
The crash conversation exercised every generated event type, including Stop
and Interrupt, in both in-process and managed app-server modes. The only
matching observer was killed before emitting output, including the empty JSON
normally returned by the Stop observer. Approval, answering, interruption,
compaction, and resume still completed. The daemon reported zero hook
submissions and left collection NotRegistered.
The standalone helper tests cover malformed, oversized, and stalled input with
neutral output.

## Antigravity CLI 1.5.17 on Linux

Recorded on Linux with `/home/dosang/.local/bin/agy` (`1.5.17`) running in Corvid
PTYs against a synthetic loopback Gemini API (`modelProvider: "gemini"`,
`GOOGLE_GEMINI_BASE_URL`).

| Event | Observed fields and Corvid behavior |
|---|---|
| `SessionStart` | Fires on the first user prompt (not at idle TUI startup) with `conversationId` and non-empty `lastUserInput`. Corvid establishes the session context. |
| `PreInvocation` | Fires before each model call with `conversationId`, `invocationNum`, `initialNumSteps`, and `lastUserInput` (no `turn_id`). Corvid maps this to `ModelRequested` and assigns a daemon-local turn key (`local-1`, `local-2`, ...). On `--conversation <id>` resumes, `SessionStart` is omitted and the first `PreInvocation` establishes the session context. |
| `PostInvocation` | Fires after each model call completes. Maps to `ModelAttemptFinished` under the current local turn. |
| `Stop` | Fires on loop termination with `conversationId`, `terminationReason`, `error`, `fullyIdle`, and `continuationActive`. When `fullyIdle` is `false`, Corvid ignores the boundary. When `fullyIdle` is `true`, `ERROR` / `MAX_STEPS_EXCEEDED` / non-empty `error` maps to `ResponseFailed`, `CANCELLED` / `INTERRUPTED` / `USER_CANCELLED` maps to `TurnInterrupted`, and normal completion (`NO_TOOL_CALL`, etc.) maps to `ResponseObserved`. |
| Subagent (`invoke_subagent`) | Fires `SessionStart`, `PreInvocation`, `PostInvocation`, and `Stop` with a distinct `conversationId` and `lastUserInput: ""`. `corvid observe --provider antigravity` discards hooks with empty `lastUserInput` without transmitting prompt text so subagents do not quarantine the parent run. |
| `PreToolUse` (excluded) | Empirically fails closed in `agy 1.5.17` when the hook binary is missing, crashes, or emits `{}`. Excluded per §6.4.6-7; passive permission and question waits use `agy`'s built-in OSC 7501 `ProgramStatus` reports instead. |


### Reproduce

```sh
cargo build --locked -p corvid-tui -p corvid-daemon
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid-tui/tests/antigravity_qualification.py --corvid target/debug/corvid
```
