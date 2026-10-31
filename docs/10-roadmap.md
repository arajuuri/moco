# Roadmap

Each phase ends with a demonstrable check. Test each on a real 10.14 machine or VM, since compatibility is the entire point.

## Phase 0: platform spike (1-3 days)
- Pick toolchain (`01`). Build "hello" with `MACOSX_DEPLOYMENT_TARGET=10.14`; confirm with `otool -l` and by running on Mojave.
- Prove: HTTPS streaming (SSE) to `api.anthropic.com` and an OpenAI-compatible endpoint from Mojave; SQLite; spawning a shell; reading the system trust store.
- Secrets: decided, file (`auth.json`, mode 0600), no Keychain; it must work without a GUI session.
- **Gate**: if TLS/cert validation fails on 10.14, bundle a TLS stack and CA bundle now.

## Phase 1: minimal agent
- Store: project, session, message, parts (`02`).
- One provider protocol (Anthropic Messages or OpenAI Chat) with streaming and tool calls, plus error classification and retry (`03`, `07`).
- Tools: `read`, `write`, `edit`, `shell`, `glob`, `grep` with truncation and external-directory checks (`04`).
- Permission engine with ask/allow/deny, `once/always/reject`, saved rules (`05`).
- Agents `build` and `plan` and the base system prompt (`06`).
- UI: minimal chat (terminal or window), permission prompt, interrupt.
- **Check**: ask "add a function to X and run the tests"; it reads, edits, runs shell with an approval prompt; kill the app mid-run and restart: session intact.

## Phase 2: daily-driver
- Inbox with steer/queue, resume after crash (`02`).
- Compaction auto/manual/overflow recovery (`03`).
- AGENTS.md, config loading (JSONC, merge, variables), model picker, cost/usage display (`07`, `08`).
- `patch`, `webfetch`, `todowrite`, `question`, subagent tool and `general`/`explore` agents.
- Snapshots + revert/undo, session diff view.
- Title generation, session list, fork, rename, archive.
- Second provider protocol (OpenAI-compatible) so many hosted/local models work; models.dev catalog.

## Phase 3: extensibility
- Skills, custom commands, `@` mentions and attachments (images/PDF).
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
