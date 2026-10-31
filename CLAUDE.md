# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## State of the repo

moco is a native coding agent for **macOS 10.14 (Mojave)**, a clean-room reimplementation of opencode (upstream commit `6c24ec9532`, MIT). Currently **specification only**: there is no source code, build system, linter or test suite yet, and no commits. Work here means reading/writing the docs in `docs/` (or, once started, implementing them per the roadmap).

Source paths cited inside the docs are relative to an opencode checkout at that commit, not to this repo.

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
