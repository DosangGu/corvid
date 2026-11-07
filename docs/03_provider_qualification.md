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
hook exits successfully with empty stdout. The production collector, its
failure behavior, and live delivery through the daemon require separate tests.
Question cancellation, plan approval, subagent ownership, compaction, and
resume still need version-specific qualification.
