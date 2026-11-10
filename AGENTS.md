# Repository Guidelines

## Project Structure & Module Organization

Corvid is a Rust coding-agent multiplexer. Read `docs/00_project_design.md`
before architectural changes. The `crates/corvid-tui/` package builds the
`corvid` command and TUI. The workspace also has `corvid-daemon` for PTYs
and daemon services, `corvid-client` for IPC and TTY,
`corvid-core` for domain types, `corvid-companion` for controller state,
`corvid-desktop` for the native slot board,
`corvid-protocol` for Protobuf and local transport, and `spikes/terminal/` for terminal
qualification.

## Build, Test, and Development Commands

Install `tic` for Linux builds. The schema reproducibility fixture uses a
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

## Testing Guidelines

Keep Rust unit tests beside modules and integration tests under each
crate's `tests/`. Name tests after behavior, e.g. `rejects_stale_epoch`. Follow the design's
acceptance matrix: identity, reconnection, input fencing, and malformed frames. No coverage
threshold is configured. Distinguish executed checks from planned tests and unverified
runtime compatibility.

## Commit & Pull Request Guidelines

History uses `docs:`. Use concise, imperative subjects and appropriate prefixes
such as `feat:` or `fix:` for implementation. Keep commits focused. PRs should explain
the problem, resulting behavior, validation, and limitations. Link relevant issues and
acceptance IDs; include screenshots for visible UI changes.

## Architecture & Security Constraints

Preserve the daemon/client boundary: the daemon owns PTYs and authoritative state.
Use bounded Protobuf messages over Unix sockets, private Windows named pipes, or SSH stdio, raw ANSI render
payloads, and JSON persistence. External controllers must not inject answers or approvals.
Keep credentials and terminal content out of committed fixtures and routine logs.
