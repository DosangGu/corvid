# Provider Qualification

## Claude Code 2.1.318 on Linux

Recorded on Linux 7.2.14 on 2026-11-07. The fixture launches the installed
interactive CLI in a real PTY with screen-reader output. It uses a temporary
home, an isolated workspace, and a loopback Messages server that supplies synthetic
responses. Question scenarios use manual permission mode. The plan scenario
starts in plan mode and the fixture chooses the approval in the PTY.

| Event | Observed fields and behavior |
|---|---|
| `SessionStart` | Startup supplies a session ID and `source=startup`. The first prompt ID is absent. |
| `UserPromptSubmit` | Supplies a prompt ID under the current session. |
| `PreToolUse` and `PostToolUse` | The observed `AskUserQuestion` invocation and result share session, prompt, and tool-use IDs. |
| `ExitPlanMode` | After writing a draft and loading the deferred tool, the CLI shows an approval dialog. Its `PreToolUse` and successful `PostToolUse` share session, prompt, and tool-use IDs. |
| `PermissionRequest` | The same question also emits this event with the session, prompt, and tool name. The tool-use ID is absent. Require separate correlation evidence before resolving a permission observation. |
| Question dismissal | Esc closes the question. The next submission and `Stop` carry a new prompt ID. The dismissed question has no matching result, failure, or denial hook in this trace. |
| `Stop` | The response retains the prompt ID after the tool result. Other hooks can continue the turn, so this boundary provides response evidence. |
| Background `Agent` | `SubagentStart` and `SubagentStop` report the parent's session and prompt IDs with a distinct `agent_id`. The parent's `PostToolUse` result reports that ID as `tool_response.agentId` with `status=async_launched`. |
| `SessionEnd` and `SessionStart` | `/clear` ends the old session with `reason=clear` and starts a distinct session with `source=clear`. The CLI stays in the same PTY. `/exit` later ends the new context with `reason=prompt_input_exit`. |
| `/compact` and `/resume` | Manual compaction emits `SessionStart` with `source=compact` and the same session ID. Switching back to a previous conversation emits `SessionEnd` with `reason=resume` for the outgoing session, followed by `SessionStart` with `source=resume` for the target. Both sides report the same prompt ID. |

Use the [Claude hook reference](https://code.claude.com/docs/en/hooks) for the
documented payloads and the [Messages streaming reference](https://platform.claude.com/docs/en/build-with-claude/streaming)
for the fixture server's response format.

## Reproduce

```sh
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid/tests/claude_qualification.py --scenario answer
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid/tests/claude_qualification.py --scenario clear
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid/tests/claude_qualification.py --scenario resume
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid/tests/claude_qualification.py --scenario cancel
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid/tests/claude_qualification.py --scenario plan
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid/tests/claude_qualification.py --scenario child
```

The [fixtures](../crates/corvid/tests/fixtures/claude/2.1.318/) keep allowlisted
fields and replace captured IDs with deterministic placeholders. The capture
hook exits successfully with empty stdout.

## Corvid Collector

Generate the exec-form hook settings with `corvid hooks claude` and merge the
generated entries into the existing arrays. A fresh `corvid attach --agent claude`
uses the daemon's PATH. The daemon probes the release before launch and verifies the
provider's executable and process incarnation on submission. The hook must run
as a direct child of the managed CLI.

The current adapter records submitted prompts with inferred confidence and
`Stop` as response evidence. It correlates questions and plan approvals by
session, prompt, and tool IDs. Permission observations without a tool ID remain
unverified. A new prompt makes unresolved requests from the previous prompt
unverified. An exact result can still settle those requests.
A confirmed `/clear` boundary gets a fresh run under the same terminal. An observed
manual compaction retains the run and its request records. During an in-process
resume, Corvid fences focus on the outgoing run and matches the start to the end
by prompt ID within five seconds. A matching pair creates a new run in the same
terminal with an empty request history. If the launch has seen that session ID
before, the new run is navigable and its activity remains uncertain. Later hooks
lack a context generation, so Corvid quarantines their activity and request
updates for the reused session.

```sh
cargo build --locked
export PYTHONDONTWRITEBYTECODE=1
python3 crates/corvid/tests/claude_qualification.py --corvid target/debug/corvid --scenario clear
python3 crates/corvid/tests/claude_qualification.py --corvid target/debug/corvid --scenario resume
python3 crates/corvid/tests/claude_qualification.py --corvid target/debug/corvid --scenario cancel
python3 crates/corvid/tests/claude_qualification.py --corvid target/debug/corvid --scenario plan
python3 crates/corvid/tests/claude_qualification.py --corvid target/debug/corvid --scenario child
python3 crates/corvid/tests/claude_qualification.py --corvid target/debug/corvid --scenario forged-output
python3 crates/corvid/tests/claude_qualification.py --corvid target/debug/corvid --scenario offline-pair
python3 crates/corvid/tests/claude_qualification.py --corvid target/debug/corvid --fault missing-helper
python3 crates/corvid/tests/claude_qualification.py --corvid target/debug/corvid --fault missing-collector
python3 crates/corvid/tests/claude_qualification.py --corvid target/debug/corvid --scenario plan --fault missing-helper
python3 crates/corvid/tests/claude_qualification.py --corvid target/debug/corvid --scenario cancel --fault missing-collector
for fault in timeout collector-saturated malformed-result; do
  python3 crates/corvid/tests/claude_qualification.py --corvid target/debug/corvid --fault "$fault"
done
python3 crates/corvid/tests/claude_qualification.py --corvid target/debug/corvid --scenario plan --fault collector-saturated
```

The managed PTY test observed a pending question, its matching resolution, a
separately unverified permission request, and a replacement run. After the local
companion restarted, it restored the slot and revalidated the live TUI binding.
A fresh virtual-slot click selected the pending run from a shell tab. The TUI
redrew the question, which stayed pending after the companion acknowledged its
alert. A click captured before `/clear` failed once the slot held the new run. A process
outside the qualified CLI used the test launch scope and left the pending count
unchanged. Claude completed the fixture question and response with either the
observer executable or the collector socket absent. The helper also passed
isolated checks for malformed JSON, oversized input, and an open stalled pipe.
The A26 fixture compacted its first conversation, cleared to a second one, then
resumed the first by its exact session ID. The Claude PID and Corvid terminal
stayed the same across both context switches. The original resolved question
remained in history, and the resumed run accepted focus through a fresh slot.

The plan fixture observed a typed pending approval and its exact resolution.
The dismissal fixture observed the same run move from one pending question to
an unverified question after the next submission. The approval completed with
the observer absent, and dismissal completed with the collector socket absent.

For A5 and A7, the offline pair fixture stopped the companion before submitting
prompts to two Claude Code 2.1.318 sessions in the same directory. Both TTYs
showed pending questions with the same provider tool-use ID and distinct
Corvid RequestIds. A fresh companion snapshot restored both slot placements
and displayed both pending questions under the same daemon epoch. The hook
trace was unchanged across resync. Answering the first question resolved only
its RequestId. The second stayed pending, and the uncorrelated permission
records stayed Unverified.

The A17 timeout fixture observed Claude terminate the stalled Stop helper and
complete the interactive turn. The typed question result had already arrived.
The saturation fixture confirmed 32 admitted handshakes and refused the next
connection before each observer invocation. Both the question and plan approval
completed in the CLI while Corvid received no usable observations. Loss before
the daemon receives a frame can leave collection coverage unchanged.
`corvid control collector-status` reports the refused connections from the
saturation fixture. The run's last recorded collection state is NotRegistered.

For A22, the fixture disconnected the local companion while a question was
pending. It sent the result's hook metadata to the observer with an invalid
tool-use ID. The verified observer reported a collection gap. A fresh
companion snapshot retained the same run, slot, and slot revision with
`GapDetected` and the unverified question. The separately unverified permission
record remained present.
The collector summary changed from one Healthy run to one GapDetected run.

The A9 child fixture ran a background `Agent` while the parent waited on an
interactive question. The child became visible after the start and the parent's
tool result agreed on session, prompt, and actor IDs. `SubagentStop` closed the
child's work and recorded its response while the parent's question stayed
pending. The companion assigned a slot only to the parent. The child record
has unknown OS process liveness and no independent focus target. Its context
retires with the parent.

Early child responses have a five-second correlation window. The inventory
reports a child coverage gap if the registry reaches its child observation
limit. Use the control snapshot for inventories larger than one control frame.
Exact manual cancellation, plan rejection, and synchronous or nested child
ownership remain unqualified.

The A18 fixture writes raw bytes to the managed Claude PTY while a real
question is pending. It sends a title with shell syntax, clipboard set and
query operations, a window resize, and a notification containing a forged
result with the real question's correlation IDs. It also draws shell syntax
and hook-shaped text using a bidirectional override. The outer TTY receives
literal text after display filtering. The registered view, slot capture,
request counts, and exact attention records match their pre-injection values.
The question resolves after the fixture enters the answer in the TTY.

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
| `Stop` | Reports a turn and `stop_hook_active`. Corvid records the first response boundary for a turn. The hook emits a fixed empty JSON object and does not request continuation. |
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

### Install the observer

Run `corvid hooks codex` on the execution host. Merge the generated groups into
the corresponding arrays in `~/.codex/hooks.json`. Review the exact generated
commands in the CLI and trust them through the normal prompt or `/hooks`.

```sh
corvid attach --agent codex
corvid attach --agent codex -- resume <session-id>
corvid control agents
```

For the qualified release, Corvid passes `-c features.daemon_auto_start=false`
to keep each managed launch independent. Later CLI arguments can override the
setting. With `--enable daemon_auto_start`, a broker can supply observations
when its executable and arguments match the qualified local app server and it
is a direct child of the launcher. An already running broker can serve the
CLI while the new Corvid run has no verified hook producer. The observer
inherits the host, epoch, and launch scope. Corvid verifies the launcher and
pins the producer's PID, start time, and executable identity. An unqualified
release reports process-only coverage.

### Reproduce

```sh
cargo build --locked
export PYTHONDONTWRITEBYTECODE=1
python3 crates/corvid/tests/codex_qualification.py --corvid target/debug/corvid
python3 crates/corvid/tests/codex_qualification.py --corvid target/debug/corvid --conversation
python3 crates/corvid/tests/codex_qualification.py --corvid target/debug/corvid --parallel
python3 crates/corvid/tests/codex_qualification.py --corvid target/debug/corvid --enable-auto-start
for fault in missing-helper missing-collector timeout collector-saturated; do
  python3 crates/corvid/tests/codex_qualification.py --corvid target/debug/corvid --fault "$fault"
done
for fault in missing-helper missing-collector; do
  python3 crates/corvid/tests/codex_qualification.py --corvid target/debug/corvid --conversation --fault "$fault"
done
```

The [allowlisted traces](../crates/corvid/tests/fixtures/codex/0.167.0/) use
fixture identifiers. For A5 and A7, the parallel fixture uses loopback OpenSSH
10.6p1 and temporary keys. It kills only the companion's SSH process and
holds the relay offline while two same-directory Codex runs display questions
with the same tool-use ID. Reconnection keeps the companion and daemon epochs
and the two slot placements. It recovers both Unverified question records
without adding hook events. The companion focuses the exact registered TUI
and acknowledges its alert while the question stays visible. Answering that
question resolves only its RequestId. Interrupting the other TUI cancels the
other RequestId. The failure tests exercised a real approval and completed its
response despite the missing or timed-out observer, missing socket, and 32
occupied collector connections. The saturation test also checked that the next
connection was refused. The conversation fixture completed
compaction and resume with either the helper or the collector socket absent.
The standalone helper tests cover malformed, oversized, and stalled input with
neutral output.
