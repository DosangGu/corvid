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
