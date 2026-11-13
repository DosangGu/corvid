# Repository Guidelines

## Project Structure & Module Organization

Corvid is a Rust coding-agent multiplexer. Read `docs/00_project_design.md`
before architectural changes. The `crates/corvid-tui/` package builds the
`corvid` command and TUI. The workspace also has `corvid-daemon` for PTYs
and daemon services, `corvid-client` for IPC and TTY,
`corvid-core` for domain types, `corvid-companion` for controller state,
`corvid-presentation` for shared presentation clients,
`corvid-protocol` for Protobuf and local transport, and `spikes/terminal/` for terminal
qualification. The Desktop app is in `crates/corvid-desktop/`, Deck in
`spikes/deck/`, and the separately launched Corvid Agent Deck in `plugins/agent-deck/`.
The root `Cargo.toml` defines the workspace package version and direct dependency
versions. Workspace members inherit those entries and share the root `Cargo.lock`.

## Build, Test, and Development Commands

Install `tic` for Linux builds. Full workspace builds on Linux also need the
[Desktop system packages](docs/04_companion.md#linux-desktop-quick-start).
The schema reproducibility fixture uses a
pinned `protoc` archive for comparison. Run `git diff --check` for whitespace.
From the workspace root:

- `cargo build --workspace`: compile workspace packages.
- `cargo test --workspace`: run unit and integration tests.
- `cargo fmt --all -- --check`: verify Rust formatting.
- `cargo clippy --workspace --all-targets -- -D warnings`: reject lint warnings.

## Coding Style & Naming Conventions

Write documentation in English and distinguish proposals from implemented behavior.
For Rust, use rustfmt defaults, four-space indentation, `snake_case` functions/modules,
`UpperCamelCase` types, and `SCREAMING_SNAKE_CASE` constants. Keep domain types independent
of generated Protobuf messages. Never hand-edit generated code or reuse removed Protobuf
field numbers; reserve them instead.

- Put statements on separate lines; separate logical stages with blank lines.
- Wrap long signatures, calls, chains, and conditions using rustfmt. Name intermediate
  values to clarify complex expressions.
- Give each function one coherent responsibility. Extract descriptive helpers for
  distinct stages, repeated logic, or complex branches; keep orchestration readable.
- Prefer early returns over unnecessary nesting. Split by responsibility, not arbitrary
  line counts; avoid helpers that merely rename trivial expressions.
- Review readability beyond formatter checks.

## Task Scope & Follow-up Work

Keep changes focused on the current task's objective. When you discover problems
or improvements outside that objective, leave an actionable TODO comment near
the relevant code and defer implementation to a follow-up task. Use the file's
comment syntax with `TODO: <unique, specific title>` so the TODO-to-issue workflow
can track it. Keep the complete title on the same physical line as `TODO:`; never
wrap it or start the description on that line. Shorten long titles instead of
wrapping them, even when formatting nearby comments. Put the context and next
steps in separate comment lines immediately below; those lines become the issue
body and may wrap normally. Preserve any `Issue URL:` line when editing a tracked
TODO so it stays linked to the existing issue. Complete any changes required for
the current task to work correctly as part of the current task.

## Testing Guidelines

Keep Rust unit tests beside modules and integration tests under each
crate's `tests/`. Name tests after behavior, e.g. `rejects_stale_epoch`. Follow the design's
acceptance matrix: identity, reconnection, input fencing, and malformed frames. No coverage
threshold is configured. Distinguish executed checks from planned tests and unverified
runtime compatibility.

## Commit & Pull Request Guidelines

Start each independent task in a dedicated branch and Git worktree based on the
latest `origin/main`. Make and validate the task's changes in that worktree,
preserving unrelated work in other worktrees.

Push the task branch and open one PR per coherent unit of work against `main`.
Do not commit or push task changes directly to `main`. Hand off the PR URL with
validation results and any limitations, and leave the PR open for review unless
the user requests a merge. Follow the PR's CI checks through completion and fix
failures before declaring it ready for review. Report any checks that remain
pending or blocked explicitly.

History uses `docs:`. Use concise, imperative subjects and appropriate prefixes
such as `feat:` or `fix:` for implementation. Keep commits focused. PRs should explain
the problem, resulting behavior, validation, and limitations. Link relevant issues and
acceptance IDs; include screenshots for visible UI changes.

Use the exact version tag as the GitHub Release title, such as `v0.1.2` or
`v0.2.0-rc.1`. Do not add `Corvid`, a product name, or any other prefix or suffix.

## Architecture & Security Constraints

Preserve the daemon/client boundary: the daemon owns PTYs and authoritative state.
Use bounded Protobuf messages over Unix sockets, private Windows named pipes, or SSH stdio, raw ANSI render
payloads, and JSON persistence. External controllers must not inject answers or approvals.
Keep credentials and terminal content out of committed fixtures and routine logs.
