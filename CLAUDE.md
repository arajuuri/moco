# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## State of the repo

moco is a native coding agent for **macOS 10.14 (Mojave)**, a clean-room reimplementation of opencode (upstream commit `6c24ec9532`, MIT). `docs/` is the specification. Phase 0 (platform spike, `spike/`) is done and Phase 1 (minimal agent) is implemented in a Cargo workspace; see `docs/10-roadmap.md` for phases and results.

Source paths cited inside the docs are relative to an opencode checkout at that commit, not to this repo.

## Commands

```
cargo test                       # all tests (engine tests use a scripted fake provider, no network)
cargo test -p moco-core shell    # filter by name
cargo build --release            # produces target/release/moco
moco -p "prompt"                 # one-shot; `moco` alone is the interactive chat; --continue resumes
```

The API key comes from `ANTHROPIC_API_KEY` or `~/Library/Application Support/moco/auth.json` (`{"anthropic": {"key": "..."}}`, mode 600; `MOCO_HOME` overrides the directory).

**Always set `CARGO_TARGET_DIR` outside the repo.** `/home/ubuntu/Development` is an sshfs mount of the Mac Pro's disk, so the repo is shared between the Linux dev box and the Mojave test machine. Building into the shared `target/` fails intermittently over sshfs and mixes Linux and macOS artifacts. Use `CARGO_TARGET_DIR=/home/ubuntu/.cache/moco-target` on Linux and `CARGO_TARGET_DIR=$HOME/moco-target` on the Mac.

Mojave test machine: `ssh claude@semis-mac-pro.lan` (macOS 10.14.6, Xcode 11.3.1, Rust via rustup). Build there with `MACOSX_DEPLOYMENT_TARGET=10.14` and check `otool -l <binary> | grep minos` (must be 10.14). Run `cargo test` on both machines for changes touching processes, paths or files. Do real-API experiments in a scratch directory on the Mac, not inside the synced repo.

## Code layout

`crates/moco-core` is the UI-independent engine; `crates/moco` is the line-based terminal front end (`Ui` trait implementation); `spike/` is the standalone Phase 0 probe crate (excluded from the workspace).

- `engine.rs`: step loop (claim, deliver inbox, run step, continue while tool calls). Tools run on scoped threads as soon as their call completes. Retries only when no tool ran in the attempt; the partial output is discarded.
- `provider/`: neutral request/event types, `anthropic.rs` (SSE parser, error classification), `retry.rs`, `fake.rs` (scripted provider for tests).
- `tools/`: read, write, edit, glob, grep, shell. Tools call `ToolCtx::permit`/`check_external`; the engine's `EnginePermitter` evaluates rules (`permission.rs`) and prompts through `Ui::ask`. `shellparse.rs` turns a command line into permission resources and "always" patterns.
- `store.rs`: SQLite (WAL, migrations in `MIGRATIONS`), inbox delivery semantics, saved permissions, execution claim and crash recovery. `chat.rs` rebuilds provider messages from stored history and repairs dangling tool calls.

Known gaps versus the docs: no compaction, config files, AGENTS.md, snapshots/undo, background shell, image/PDF reads, or title generation (Phase 2+). The shell arity table in `shellparse.rs` is partial. The default ruleset allows everything except `.env` reads, as upstream does, and there is no config yet to change it.

## Reading order

`docs/00-overview.md` (scope, architecture, conventions) → `docs/10-roadmap.md` (phased build order, acceptance checks, test strategy) → the topical docs:
`01` platform/toolchain, `02` domain model + SQLite schema, `03` agent loop, `04` tools, `05` permissions, `06` agents and prompts, `07` providers, `08` config/extensions, `09` UI.

## Key constraints and decisions

- **Platform is the whole point**: nothing from upstream's runtime (Bun, Electron, current Node, Go 1.21+) loads on 10.14. No SwiftUI/Combine/async-await. Intel x86_64 only; build with `MACOSX_DEPLOYMENT_TARGET=10.14` and verify every shipped Mach-O with `otool -l` (`minos` must be <= 10.14; CI should fail otherwise). `01` recommends a Rust core (engine + TUI, testable on Linux) with an optional AppKit shell later; Swift+AppKit is the alternative. The platform doc is explicitly unverified against a real Mojave machine, so toolchain choice is gated on the Phase 0 spike (TLS/cert validation, SSE streaming, SQLite, process spawning).
- **Architecture**: UI-independent, in-process engine (session engine / tool runtime / provider clients / SQLite store + config loader) with an in-process event stream. Do not build upstream's HTTP/SSE client-server API, plugin system, LSP, event-sourcing, etc. (see "Deliberately dropped" in `10-roadmap.md`).
- **Conventions to preserve**: one step = one LLM call and history is reloaded from the store each step; prompts are persisted before execution (crash-resumable); tool output is bounded (2000 lines / 50 KiB, remainder saved to file); permissions are a flat ordered rule list, last match wins, default `ask`; IDs are prefixed, time-ordered (`msg_`, `ses_`, `per_`, `evt_`).
- **Testing approach (planned)**: scripted fake provider replaying recorded SSE streams; table-driven golden tests for permissions, edit fuzzy matching, patch parser, wildcard matcher, shell arity extraction; crash-recovery test via SIGKILL.

## Licensing

Docs quote upstream prompts, tool descriptions and tables verbatim. Keep `LICENSE-opencode` and `NOTICE` intact, and retain the MIT notice in any file that copies substantial upstream text. Don't reuse upstream names or logos for the product.
