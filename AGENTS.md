# Repository Guidelines

Corvid is a Rust coding-agent multiplexer.
See [docs/README.md](docs/README.md) for the full documentation index.

## Workspace Overview

- `crates/corvid-tui/`: `corvid` CLI and TUI.
- `crates/corvid-daemon/`: PTYs and daemon services.
- `crates/corvid-client/`: IPC and TTY client.
- `crates/corvid-core/`: domain types.
- `crates/corvid-companion/`: controller state.
- `crates/corvid-presentation/`: shared presentation clients.
- `crates/corvid-protocol/`: Protobuf schemas and local transport.
- `crates/corvid-async/`: shared async runtime helpers.
- `crates/corvid-update/`: self-update and release verification.
- `crates/corvid-desktop/`: Corvid Desktop GUI.
- `crates/corvid-agent-deck/` & `crates/corvid-agent-deck-core/`: Agent Deck UI and core state.
- `plugins/agent-deck/`: separately launched Corvid Agent Deck binary.
- `deploy/cargo-desktop/`: Desktop/installer packaging helper.
- `spikes/terminal/`: terminal qualification spike.

The root `Cargo.toml` defines the workspace package version and direct
dependency versions; workspace members inherit them and share `Cargo.lock`.

## Build, Test, and Lint Commands

Install `tic` for Linux builds. Full workspace builds on Linux also require the
[Desktop system packages](docs/guides/desktop.md#linux-desktop-quick-start).
The schema reproducibility fixture uses a pinned `protoc` archive. Run
`git diff --check` for whitespace. From the workspace root:

- `cargo build --workspace`: compile workspace packages.
- `cargo test --workspace`: run unit and integration tests.
- `cargo fmt --all -- --check`: verify Rust formatting.
- `cargo clippy --workspace --all-targets -- -D warnings`: reject lint warnings.

## Core Workflow and Scope

- Start each independent task in a dedicated branch and Git worktree based on
  the latest `origin/main`; never commit or push directly to `main`.
- Open one PR per coherent unit of work, follow CI checks to completion, and
  leave the PR open for review unless the user requests a merge. Clean up local
  build artifacts, temporary files, and the dedicated worktree after completion.
- Keep changes focused on the current task's objective; defer out-of-scope
  discoveries with `TODO: <unique, specific title>` comments.

## Architecture and Security Invariants

- Preserve the daemon/client boundary: the daemon owns PTYs and authoritative
  state.
- Use bounded Protobuf messages over Unix sockets, private Windows named pipes,
  or SSH stdio, raw ANSI render payloads, and JSON persistence.
- Keep domain types independent of generated Protobuf messages. Never hand-edit
  generated code or reuse removed Protobuf field numbers; reserve them instead.
- External controllers must not inject answers or approvals.
- Keep credentials and terminal content out of committed fixtures and routine logs.

## Documentation & Guideline Router

Read the relevant guide before making changes in that area:

| When working on... | Read |
| --- | --- |
| Architecture, state ownership, lifecycle, slots, or roadmap | [docs/architecture/overview.md](docs/architecture/overview.md) |
| IPC, Protobuf schema, SSH transport, or snapshot/watch sync | [docs/architecture/protocol.md](docs/architecture/protocol.md) |
| PTYs, terminal emulation, rendering, input, or overload | [docs/architecture/terminal-runtime.md](docs/architecture/terminal-runtime.md) |
| Provider adapters, hooks, AgentMonitor, or WezTerm focus | [docs/architecture/providers-and-focus.md](docs/architecture/providers-and-focus.md), [docs/guides/providers.md](docs/guides/providers.md) |
| Rust style, naming, testing conventions, or acceptance matrix | [docs/contributing/coding-and-testing.md](docs/contributing/coding-and-testing.md) |
| Commits, PRs, TODO-to-issue format, or GitHub Release titles | [docs/contributing/workflow-and-prs.md](docs/contributing/workflow-and-prs.md) |
| Desktop or TUI user-facing workflows and UX parity | [docs/contributing/presentation-ux.md](docs/contributing/presentation-ux.md) |
| CI workflows, local UI/TTY/SSH checks, or flaky CI tracking | [docs/contributing/ci-and-validation.md](docs/contributing/ci-and-validation.md) |
| User setup, Desktop, Daemon, Agent Deck, or releases | [docs/README.md](docs/README.md) |
