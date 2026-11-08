# moco: clean-room spec derived from opencode (upstream commit 6c24ec9532)

Goal: a native coding-agent application that runs on **macOS 10.14 (Mojave)** with a feature set similar to opencode.
opencode is MIT licensed, so these documents describe its behavior closely, including prompts and schemas,
with source file references so each statement can be re-checked. The implementer should still write new code and should
keep the MIT notice for any text copied verbatim (system prompts, tool descriptions).

Why a rewrite: every Bun release checked (0.6 to 1.3) ships macOS x64 binaries with a minimum OS of 11.0 or 13.0, and the repo
pins Electron 44 (macOS 12+). Nothing in this repo's runtime stack can be made to load on 10.14.

## Document map

| File                          | Contents                                                           |
|-------------------------------|--------------------------------------------------------------------|
| `01-platform-macos-1014.md`   | What 10.14 allows, language/toolchain options, recommendation      |
| `02-domain-model.md`          | Sessions, messages, inbox, events, storage schema                  |
| `03-agent-loop.md`            | Step loop, tool execution, retry, max steps, compaction, subagents |
| `04-tools.md`                 | Every built-in tool: name, input schema, behavior, limits          |
| `05-permissions.md`           | Rule model, evaluation, shell command scanning, replies            |
| `06-agents-and-prompts.md`    | Built-in agents, system prompts, reminders, title/summary prompts  |
| `07-providers.md`             | Provider abstraction, wire protocols to implement, model catalog   |
| `08-config-and-extensions.md` | Config files, instructions (AGENTS.md), skills, commands, MCP      |
| `09-ui.md`                    | Screens, interactions, key bindings                                |
| `10-roadmap.md`               | Phased build order with acceptance checks and results per phase    |
| `11-usage.md`                 | User reference for the app as built: commands, config, storage     |
| `12-tui-plan.md`              | Plan and status of the full-screen terminal UI                     |
| `13-architecture.md`          | The code as built: modules, run lifecycle, storage, invariants     |
| `14-macos-10.7.md`            | The `macos-10.7` branch: Rust 1.73 pins, back-ports, building for Lion |

## Scope

In scope (phases 1-3, see roadmap): single-user local app; sessions persisted to SQLite; streaming chat with tool calls;
the file/shell/search/web tools; permission prompts; agents (build, plan, general, explore); compaction; AGENTS.md;
skills; custom commands; MCP (stdio and remote); providers Anthropic, OpenAI-compatible, OpenAI Responses, Gemini;
snapshots/undo.

Out of scope (the upstream repo has them; do not build first): the client/server HTTP API with ~170 endpoints, plugin system
(Effect plugins, GUI plugins), Code Mode, remote workspaces, schedules, artifacts, voice, image/video generation,
sharing/enterprise, pairing/tunnels, persistent PTY handoff, LSP, formatters, event-sourced replay.
These are listed in section "Deliberately dropped" in `10-roadmap.md`.

## Architecture to build

```
UI (AppKit GUI and/or terminal UI)
        |  in-process calls + event stream
Session engine  -- agent loop, inbox, compaction, permissions, snapshots
        |                      |                  |
Tool runtime           Provider clients       Store (SQLite)
(read, edit, shell,    (Anthropic, OpenAI,    config loader
 grep, MCP, ...)        Gemini, compat)
```

Keep the engine UI-independent and process-local: the UI subscribes to an in-process event stream and issues commands.
Upstream's HTTP/SSE boundary exists for multiple clients; skip it unless you want a remote UI later (then the endpoint list
in `packages/protocol/src/groups/*.ts` is the reference).

## Conventions from upstream worth keeping

- One **step** = one logical LLM call. Tool loop is not in memory: every step reloads history from the store and continues.
- Prompts are admitted durably first, executed second. Execution can be retried or resumed after a crash.
- Tool results are bounded before the model sees them (2000 lines / 50 KiB, rest saved to a file).
- Permissions are a flat ordered rule list, last match wins, default `ask`.
- IDs are prefixed and sortable (`msg_`, `per_`, `evt_`, `ses_`...) and generated ascending (time-ordered).
