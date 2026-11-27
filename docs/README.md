# Corvid Documentation

Welcome to the Corvid documentation. Guides are organized by audience and purpose.

## User & Operator Guides

- [Daemon Guide](guides/daemon.md) — Local Linux/Windows daemon usage, session restoration, systemd setup, and SSH notifications
- [Corvid Desktop Guide](guides/desktop.md) — Linux, Windows, and macOS Desktop setup, SSH connections, terminal preferences, and WezTerm pairing
- [Corvid Agent Deck](guides/agent-deck.md) — Built-in Agents window, layouts, slot queue, tray/menu-bar items, and notifications
- [Provider Hooks and Observers](guides/providers.md) — Claude Code, Codex CLI, and Antigravity CLI hook setup and Corvid Collector
- [Release Distribution, Installation, and Updates](guides/release-and-install.md) — Installers, Cargo install, macOS DMG, Windows Setup, updates, and publishing

## Architecture & Specifications

- [Architecture Overview](architecture/overview.md) — Product boundary, locked decisions, state ownership, identity, slots, persistence, and roadmap
- [IPC, Protocol, and Snapshot Consistency](architecture/protocol.md) — Discovery, SSH transport, Public Protocol v1, framing, schema evolution, and atomic barriers
- [Terminal Runtime, Rendering, and Input](architecture/terminal-runtime.md) — PTY/screen pipeline, writer ownership, input protocols, and overload bounds
- [Provider Adapters and Exact Focus](architecture/providers-and-focus.md) — Adapter contracts, observation-only hooks, foreground detection, and FocusAgent/WezTerm registration

## Contributors & AI Agents

- [Repository Guidelines (`AGENTS.md`)](../AGENTS.md) — Always-on core rules, workspace map, build/lint commands, and task router
- [Coding and Testing Guidelines](contributing/coding-and-testing.md) — Rust style, Protobuf rules, test conventions, and the Acceptance Matrix
- [Workflow, PRs, and TODOs](contributing/workflow-and-prs.md) — Worktree workflow, commit/PR conventions, TODO-to-issue format, and release titles
- [Presentation UX](contributing/presentation-ux.md) — Desktop/TUI parity rules and shared TUI/GUI behavior contract
- [Continuous Integration and Validation](contributing/ci-and-validation.md) — PR/release CI coverage, local UI/TTY/SSH reproduction, WSL 2 checks, and flaky CI tracking

## Qualification & Benchmarks

- [Terminal Qualification](qualification/terminal.md) — Phase 0 terminal benchmarks, `libghostty-vt` runtime measurements, and Kitty graphics notes
- [Daemon and Runtime Qualification](qualification/daemon-and-runtime.md) — Compio runtime benchmarks, backpressure/latency measurements, and daemon verification logs
- [Provider Qualification](qualification/providers.md) — Captured provider traces, working-activity qualification, and local reproduction scripts
- [Platform Verification and Qualification](qualification/platforms.md) — Desktop/companion verification, ARM64/release boundaries, Agent Deck checks, macOS validation, and remaining device qualification

## Where to Place Documentation

Corvid uses a hybrid layout:

- **Root `docs/`**: Product-wide and cross-cutting documentation — user guides (`guides/`), system architecture (`architecture/`), contributor and CI guidelines (`contributing/`), qualification records (`qualification/`), and shared visual assets (`images/`, `screenshots/`).
- **Project-local `docs/` or `README.md`** (under `crates/<name>/`, `plugins/<name>/`, `deploy/`, or `spikes/<name>/`): Implementation notes, internal module design, or developer documentation specific to a single crate or subproject. Link broadly relevant local docs from `docs/README.md` (and `AGENTS.md` if agents should route to them), and exclude large or non-package docs/assets from published `crates.io` crates via `Cargo.toml`.
