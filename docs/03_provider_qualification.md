# Provider Qualification

## Claude Code 2.1.318 on Linux

Recorded on Linux 7.2.14 on 2026-11-07. The fixture launches the installed
interactive CLI in a real PTY with screen-reader output and manual permission
mode. It uses a temporary home, an empty workspace, and a loopback Messages
server that supplies synthetic responses. The fixture answers its own question
through the PTY and exits through the normal command prompt.

| Event | Observed fields and behavior |
|---|---|
| `SessionStart` | Startup supplies a session ID and `source=startup`. The first prompt ID is absent. |
| `UserPromptSubmit` | Supplies a prompt ID under the current session. |
| `PreToolUse` and `PostToolUse` | The observed `AskUserQuestion` invocation and result share session, prompt, and tool-use IDs. |
| `PermissionRequest` | The same question also emits this event with the session, prompt, and tool name. The tool-use ID is absent. Require separate correlation evidence before resolving a permission observation. |
| `Stop` | The response retains the prompt ID after the tool result. Other hooks can continue the turn, so this boundary provides response evidence. |
| `SessionEnd` and `SessionStart` | `/clear` ends the old session with `reason=clear` and starts a distinct session with `source=clear`. The CLI stays in the same PTY. `/exit` later ends the new context with `reason=prompt_input_exit`. |

Use the [Claude hook reference](https://code.claude.com/docs/en/hooks) for the
documented payloads and the [Messages streaming reference](https://platform.claude.com/docs/en/build-with-claude/streaming)
for the fixture server's response format.

## Reproduce

```sh
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid/tests/claude_qualification.py --scenario answer
PYTHONDONTWRITEBYTECODE=1 python3 crates/corvid/tests/claude_qualification.py --scenario clear
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
`Stop` as response evidence. It correlates question start and result by session,
prompt, and tool IDs. Permission observations without a tool ID remain unverified.
A confirmed `/clear` boundary gets a fresh run under the same terminal. An observed
reuse of the same session ID creates a fresh run with uncertain coverage.

```sh
cargo build --locked
export PYTHONDONTWRITEBYTECODE=1
python3 crates/corvid/tests/claude_qualification.py --corvid target/debug/corvid --scenario clear
python3 crates/corvid/tests/claude_qualification.py --corvid target/debug/corvid --fault missing-helper
python3 crates/corvid/tests/claude_qualification.py --corvid target/debug/corvid --fault missing-collector
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

Qualification remains for question cancellation, plan approval, child records,
compaction, resume, and collector saturation. Delivery failures that occur before
the daemon receives a frame can leave coverage unchanged.

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
| `notify` | A root completion can match the hook session and turn. The same executable also delivers completions with other thread IDs that have no matching lifecycle hook in this fixture. Corvid does not use `notify` to establish an interactive run's identity. |

The official references describe the [Codex Hook contract](https://learn.chatgpt.com/docs/hooks)
and [notification payload](https://learn.chatgpt.com/docs/config-file/config-advanced#notifications).
`SessionEnd` was not observed during the fixture's normal exit. Corvid uses OS
exit evidence. Resume, compaction, plan approval, and child event ownership
remain outside this release qualification.

### Install the observer

Run `corvid hooks codex` on the execution host. Merge the generated groups into
the corresponding arrays in `~/.codex/hooks.json`. Review the exact generated
commands in the CLI and trust them through the normal prompt or `/hooks`.

```sh
corvid attach --agent codex -- --disable daemon_auto_start
corvid control agents
```

The explicit `daemon_auto_start` override was tested with two independent local
interactive processes sharing a working directory and profile. The default
launch also works when its managed internal app server is a direct child with
the qualified package path and local daemon arguments. The observer inherits
the host, epoch, and launch scope. Corvid verifies the launcher and pins the
producer's PID, start time, and executable identity. A shared Codex daemon
outside that launch's verified process chain cannot contribute observations.
An unqualified release reports process-only coverage.

### Reproduce

```sh
cargo build --locked
export PYTHONDONTWRITEBYTECODE=1
python3 crates/corvid/tests/codex_qualification.py --corvid target/debug/corvid
python3 crates/corvid/tests/codex_qualification.py --corvid target/debug/corvid --parallel
for fault in missing-helper missing-collector timeout collector-saturated; do
  python3 crates/corvid/tests/codex_qualification.py --corvid target/debug/corvid --fault "$fault"
done
```

The [allowlisted traces](../crates/corvid/tests/fixtures/codex/0.167.0/) use
fixture identifiers. The parallel test held two questions with the same tool
ID in separate same-directory runs. The companion synchronized both, focused
the exact registered TUI, and acknowledged its alert while the dialog stayed
visible. Answering that question left the second request open until it was
interrupted. The failure tests exercised a real approval and completed its
response despite the missing or timed-out observer, missing socket, and 32
occupied collector connections. The standalone helper tests cover malformed,
oversized, and stalled input with neutral output.
