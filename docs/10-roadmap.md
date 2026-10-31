# Roadmap

Each phase ends with a demonstrable check. Test each on a real 10.14 machine or VM, since compatibility is the entire point.

## Phase 0: platform spike (1-3 days)
- Pick toolchain (`01`). Build "hello" with `MACOSX_DEPLOYMENT_TARGET=10.14`; confirm with `otool -l` and by running on Mojave.
- Prove: HTTPS streaming (SSE) to `api.anthropic.com` and an OpenAI-compatible endpoint from Mojave; SQLite; spawning a shell; reading the system trust store.
- Secrets: decided, file (`auth.json`, mode 0600), no Keychain; it must work without a GUI session.
- **Gate**: if TLS/cert validation fails on 10.14, bundle a TLS stack and CA bundle now.

### Phase 0 results (spike in `spike/`, run on macOS 10.14.6, Intel Xeon X5670, Xcode 11.3.1)
Toolchain decision: **Rust core** (option B in `01`). Rust 1.99.1 stable builds with `MACOSX_DEPLOYMENT_TARGET=10.14`; `otool -l` shows `minos 10.14`; the only dylibs linked are `libSystem` and `libiconv`.
- TLS: `ureq` + rustls reaches api.anthropic.com, api.openai.com, generativelanguage.googleapis.com and models.dev, and rejects an expired certificate. Both bundled Mozilla roots and the system trust store (`native-certs` feature) validate every endpoint, so the gate did not trigger. Keep the system-roots feature available plus an option to load a PEM bundle, since the OS store is frozen.
- SSE: a streaming Anthropic Messages request returned 8 events, first after about 560 ms. Not yet tested: an OpenAI-compatible stream.
- SQLite: bundled amalgamation 3.54.1 (`rusqlite` `bundled`); `RETURNING`, JSON1 and WAL work.
- Shell: `/bin/zsh -c` spawn with stdout, stderr and exit code capture, and kill of a running shell, work. `git` is present (Xcode CLT); **`rg` is not**, so bundle ripgrep (x86_64, minos <= 10.14) or implement grep and glob natively.
- Secrets: file only (see `02`). The Keychain failed over SSH with `-25308`, and a GUI-free app must not depend on it.
- Not covered: killing a whole process tree (the kill test only killed the shell), PTY, running the probes in a GUI session.

## Phase 1: minimal agent
- Store: project, session, message, parts (`02`).
- One provider protocol (Anthropic Messages or OpenAI Chat) with streaming and tool calls, plus error classification and retry (`03`, `07`).
- Tools: `read`, `write`, `edit`, `shell`, `glob`, `grep` with truncation and external-directory checks (`04`).
- Permission engine with ask/allow/deny, `once/always/reject`, saved rules (`05`).
- Agents `build` and `plan` and the base system prompt (`06`).
- UI: minimal chat (terminal or window), permission prompt, interrupt.
- **Check**: ask "add a function to X and run the tests"; it reads, edits, runs shell with an approval prompt; kill the app mid-run and restart: session intact.

### Phase 1 results (workspace `crates/moco-core` + `crates/moco`; verified on macOS 10.14.6)
Status: complete. 84 unit tests pass on Linux and on the Mojave machine; the release binary has `minos 10.14` and links only `libSystem` and `libiconv`.
- **Check, live API** (Anthropic Messages, `claude-sonnet-5-5`): asked to add `double(x: i32)` with a unit test and run `cargo test` in a scratch crate. It read `src/lib.rs`, edited it twice, ran `cargo test` through the shell tool and reported 2 passed. The resulting code was correct.
- **Check, crash recovery:** SIGKILL during a `sleep 40` shell call; `moco --continue` printed "resuming interrupted run", turned the dangling tool call into an `Tool execution interrupted` error result and the model continued from the intact history.
- **Check, permission prompt:** reading `.env` asked; rejecting without feedback stopped the run.
- **Implemented**: SQLite store with migrations, inbox (steer/queue, idempotent admission), execution claim with a bounded resume counter, saved permissions; Anthropic provider (SSE parser, HTTP and in-stream error classification, retry policy from `03`); step loop with tools running concurrently on scoped threads; `read`, `write`, `edit` (exact, typographic and trailing-whitespace matching, CRLF/BOM kept), `glob`, `grep` (native, `.gitignore` aware, because `rg` is absent), `shell` (process-group kill on timeout or interrupt, tail truncation, v1 command scanner); `build` and `plan` agents with the base prompt; line-based terminal chat.
- **Deviations from the spec**: retries only when no tool ran in the failed attempt, and the partial output is discarded (re-running would repeat side effects); the Plan reminder is appended at request time instead of being stored as a synthetic message; reasoning deltas are shown to the UI but not stored; blocking I/O on threads instead of an async runtime; the Plan agent denies all `edit` instead of allowing a plan directory.
- **Not done / caveats**: the default ruleset (`* allow` except `.env`) means `shell` and `edit` never prompt until config exists (Phase 2), so the roadmap's "approval prompt" was exercised on `.env` reads only; the shell arity table is partial; no background shell, image or PDF reads, `AGENTS.md` loading, compaction, snapshots, title generation or config files; a SIGKILLed moco leaves its shell child running; the permission prompt serializes concurrent asks but "reject" does not yet cancel other already-pending asks beyond the declined flag.
- **Dev setup lesson**: the repo folder is shared between the Linux box and the Mac over sshfs, so each machine must build into a `CARGO_TARGET_DIR` outside the repo (see `CLAUDE.md`).

## Phase 2: daily-driver
- Inbox with steer/queue, resume after crash (`02`).
- Compaction auto/manual/overflow recovery (`03`). [done; verified live on the Mojave machine with a 5k-token window, see `03` implementation notes]
- AGENTS.md, config loading (JSONC, merge, variables) [done, see `08` implementation notes], model picker and cost/usage display [done, see `07` implementation notes] (`07`, `08`).
- `patch`, `webfetch`, `todowrite`, `question`, subagent tool and `general`/`explore` agents. [done; verified live on the Mojave machine, see `04` implementation notes]
- Snapshots + revert/undo, session diff view. [done; verified live on the Mojave machine, see `03` implementation notes]
- Title generation, session list, fork, rename, archive. [done; see `02` and `06` implementation notes]
- Second provider protocol (OpenAI-compatible) so many hosted/local models work; models.dev catalog. [done; verified against a local test server and the live Anthropic API, not yet against a live OpenAI-compatible provider]

### Phase 2 results (verified on macOS 10.14.6 with the live Anthropic API)
Status: complete. 197 unit tests pass on Linux and on the Mojave machine; the release binary is `minos 10.14`. Every item below was exercised end to end on the Mac, not only in tests. Per-feature design notes and deviations are in the "implementation notes" sections of `02`, `03`, `04`, `06`, `07` and `08`.
- **Config and instructions**: layered JSONC config (`moco.jsonc`), `{env:}`/`{file:}` substitution, `AGENTS.md` (and `CLAUDE.md`) discovery including files found while reading; permission rules from config made `shell` prompt in a live run.
- **Providers**: Anthropic and OpenAI-compatible Chat Completions, models.dev catalog (embedded snapshot, `moco models --refresh` gives about 7,000 models), `/model`, `moco auth`, cost with context tiers. The OpenAI-compatible adapter is verified against a local test server only, not a live provider.
- **Prompt caching**: Anthropic `cache_control` breakpoints; a follow-up turn within five minutes cost $0.0006 instead of about $0.015.
- **Compaction**: automatic, on provider overflow and manual; a live run with a 5k-token window kept a codename across a compaction.
- **Tools and agents**: `patch`, `webfetch` (with an HTML to Markdown converter), `todowrite`, `question`, `subagent` with `general` and `explore`.
- **Snapshots**: per-step working-tree snapshots, `/undo`, `/redo`, `/diff`; an undo survives a restart.
- **Sessions**: generated titles, list, resume by id piece, rename, fork, archive.
- **Deviations worth knowing** (all documented where they arise): moco's own snapshot store instead of a private git dir; summarization, title and update prompts are my wording of the spec's contracts; the Plan reminder lives in the system prompt; steps run synchronously with a snapshot taken just before the request; subagents run in the foreground only; config files are `moco.json[c]`, not `opencode.json`.
- **Not done** (carried to later phases or dropped): skills, custom commands and `@` mentions, MCP, `websearch`, background shell and background subagents, OpenAI Responses and Gemini protocols, reasoning variants and per-model request overlays, the `summary` agent, a graphical or full-screen UI (the terminal chat is line based), notifications.

## Phase 3: extensibility
- Skills, custom commands, `@` mentions [done, see `08` implementation notes] and attachments (images/PDF).
- MCP stdio, then Streamable HTTP + OAuth.
- Background shell, formatter hooks, OpenAI Responses and Gemini protocols.
- Notifications, themes, keybinding config.

## Phase 4: nice to have
Terminal pane, worktrees, LSP diagnostics, websearch, references, multi-window, scheduled runs.

## Deliberately dropped from upstream (do not need to reproduce)
- Client/server HTTP API (about 170 endpoints in `packages/protocol/src/groups/*`), OpenAPI SDK generation, pairing, tunnels, remote workspaces, multi-client SSE feed. Add later only if you want a remote UI or SDK.
- Effect-based plugin system and GUI plugins, plugin hooks, Code Mode.
- Event-sourced aggregate replay and migration-from-v1 machinery.
- Image/video/speech/transcription generation, voice sessions, artifacts, schedules (cron), usage-limit APIs, enterprise sharing/MDM policies, console/billing site.
- The ~45 provider protocol adapters beyond the 3-5 listed in `07`.

## Test strategy
- Replace live LLMs with a **scripted fake provider** that replays recorded SSE streams (upstream has `packages/http-recorder` for this idea). Golden-test: tool call parsing, retry/backoff timings, compaction triggers, permission decisions (table-driven from `05`), edit fuzzy matching, patch parser, wildcard matcher, shell arity extraction.
- Property-test `edit`/`patch` round-trips with CRLF, BOM, unicode quotes.
- Crash-recovery test: SIGKILL during a tool call, relaunch, verify claim recovery and attempt cap.
- Binary compatibility check in CI: fail if any shipped Mach-O has `minos > 10.14`.

## Licensing
opencode is MIT. Keep the copyright notice and permission text in any file or document that copies substantial portions (prompts, tool descriptions, the arity table). Do not reuse upstream names/logos for your product.
