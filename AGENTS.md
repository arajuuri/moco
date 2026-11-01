# AGENTS.md

moco is a native coding agent for **macOS 10.14 (Mojave), Intel x86_64**: a clean-room Rust reimplementation of opencode (upstream commit `6c24ec9532`, MIT). `docs/` is the specification; read `docs/00-overview.md`, then `docs/10-roadmap.md` (phases, results, what is left). Phases 0 to 2 are done and Phase 3 (extensibility) is being built slice by slice. Source paths cited in the docs are relative to an opencode checkout, not this repo.

## Commands

```
cargo test                          # whole workspace; no network (scripted fake provider, local fake servers)
cargo test -p moco-core wildcard    # filter by test name (substring); -p moco for the front end
cargo test -p moco --test protocols # one integration test file (also: --test tui)
cargo build --release               # target/release/moco
scripts/install.sh                  # build for 10.14 and install to ~/.local/bin (--uninstall, --help)
moco -p "prompt"                    # one-shot; bare `moco` is the interactive TUI (--plain for line chat); --continue resumes
```

- There is no CI, and clippy and rustfmt are not installed on the Mojave machine. Don't reformat files wholesale.
- Integration tests in `crates/moco/tests/` and the MCP tests need `python3` (fake LLM/MCP servers, a pty driver). Without it, some skip silently and the MCP tests fail.
- **Set `CARGO_TARGET_DIR` outside the repo** (`$HOME/moco-target` on the Mac, `/home/ubuntu/.cache/moco-target` on Linux). On the Linux dev box the repo is an sshfs mount of the Mac's disk, so a shared `target/` breaks intermittently and mixes Linux and macOS artifacts.
- Mojave test machine: `ssh claude@semis-mac-pro.lan` (10.14.6, Xcode 11.3.1, rustup). Build there with `MACOSX_DEPLOYMENT_TARGET=10.14` and check `otool -l <binary> | grep minos`, which must say 10.14. Run `cargo test` on both Linux and the Mac for changes touching processes, paths or files. Do real-API experiments in a scratch directory outside the synced repo.
- Credentials: the provider's env var (e.g. `ANTHROPIC_API_KEY`) or `~/Library/Application Support/moco/auth.json` (`{"anthropic": {"key": "..."}}`, mode 600). `MOCO_HOME` moves the data directory; **always point it at a temp dir** when experimenting with auth, sessions or the catalog cache. Other overrides: `MOCO_CONFIG`, `MOCO_MODEL`, `MOCO_SHELL`, `MOCO_ASCII`. Subcommands: `moco auth|models|sessions`.
- `scripts/update-catalog-snapshot.py` regenerates `crates/moco-core/src/catalog_snapshot.json`. `scripts/tui-gallery.py` renders `tui/gallery.rs` sample screens to PNG (`MOCO_GALLERY=dir`) so you can look at a TUI change.

## Architecture

`crates/moco-core` is the UI-independent engine. `crates/moco` is the front end: line-based chat and one-shot in `main.rs`, and the ratatui UI in `tui/`. `app.rs` is a pure model (`update`/`render`, tested via `TestBackend`) and `mod.rs` is the event loop with the engine on a worker thread. Pty tests are `tests/tui.rs` + `tui_pty.py`; the plan is `docs/12-tui-plan.md`. `spike/` and `spike-tui/` are standalone probes with their own workspaces. Don't build them as part of the workspace.

Things that take several files to understand:

- **Step loop** (`engine.rs`): one step = one LLM call, with history reloaded from the store each step. Prompts are persisted before execution, so sessions are crash-resumable. Tools run on scoped threads as soon as their call completes. Retries happen only if no tool ran in the attempt, and the partial output is discarded. Tools that need engine state (todos, forms, child sessions) go through `ToolCtx::host` (`EngineHost`). Subagents are inline child sessions whose events arrive wrapped in `UiEvent::Nested`. `engine/jobs.rs` runs background shell commands and subagents (needs `Engine::share()`); completion is queued as a `synthetic` steer message, and background subagents use an `Unattended` Ui that refuses prompts.
- **Providers** (`provider/`): neutral request/event types, one adapter per wire protocol. Requests must be byte-stable prefixes across the steps of a turn, because `anthropic.rs` marks them for prompt caching, and a test enforces this. `openai_responses.rs` and `gemini.rs` are experimental and have never run against the live services. Provider-private data (reasoning items, thought signatures) travels as `Block::Native`/`Event::Native` and is replayed only by its own adapter. `fake.rs` (scripted provider) and `testserver.rs` are the test doubles.
- **Permissions**: tools call `ToolCtx::permit`/`check_external`. `EnginePermitter` evaluates a flat ordered rule list (`permission.rs`, last match wins, default `ask`) and prompts via `Ui::ask`. `shellparse.rs` turns a command line into permission resources and "always" patterns, and its arity table is partial. The default ruleset allows everything except `.env` reads, as upstream does; add `permissions` to a `moco.jsonc` to make `shell`/`edit` ask, or set `"permission_preset": "careful"` (`permission::preset_rules`; spliced in after the mode rules so agent-specific rules and the user's `permissions` still win). `sandbox` (`sandbox.rs`, config `{"enabled", "writable", "network"}`) wraps `shell` and background shells in macOS Seatbelt (`sandbox-exec`); `unsandboxed: true` on the tool asks via the `shell.unsandboxed` action, and `!command` is never sandboxed. `permission_classifier` (`classifier.rs`, `engine/classify.rs`; user config only, off by default) lets a cheap model approve routine `shell` commands that would only ask because of a *default* rule (`Rule::soft`, set on the manual mode's catch-all and the careful preset's `shell *`; `permission::only_default_asks`). It can only turn ask into allow: explicit rules, opaque commands and the `never_classify` list (sudo, `rm -rf`, `git push`, shells...) go to the user, and any timeout, error or garbled reply asks. Verdicts are cached per session by exact command, not by `save_pattern`, because `python *` or `rm *` would otherwise cover arbitrary arguments. Builds that write outside the project (e.g. `CARGO_TARGET_DIR=$HOME/moco-target`) need that path in `sandbox.writable`.
- **Anything a project config can make moco execute** (MCP servers, formatters) must go through the project-command approval (`mcp/trust.rs`, `main.rs::project_command_allowed`/`formatters_from`).
- **Extensions**: `skills::config_roots` is the shared directory list (`~/.claude`, `~/.agents`, app config, then `.claude`/`.agents`/`.moco` per ancestor) used by skills, commands and `AGENTS.md` discovery (`instructions.rs`). `@` mentions are resolved at submit time, stored on the user message, and rendered by `chat::to_chat`.
- **MCP** (`mcp/`): `client.rs` (stdio), `http.rs` (Streamable HTTP) and `oauth.rs` implement `client::McpConn`; the last two are verified only against fakes. `manager.rs` exposes tools as `<server>_<tool>`, prompts as slash commands and resources as an `mcp-resource` tool. Tools are fetched from `Manager::tools()` on every request, which is why `Tool::name` returns `&str`. Tests spawn `crates/moco-core/tests/fake_mcp*.py`.
- **Storage**: `store.rs` is SQLite (WAL, migrations in `MIGRATIONS`) plus inbox delivery, saved permissions, the execution claim and crash recovery. `chat.rs` rebuilds provider messages from history and repairs dangling tool calls. Media is stored as content-addressed blobs (`media.rs`/`blobs.rs`) and referenced from messages. `snapshot.rs` takes `git`-free working-tree snapshots before every step for undo/redo/diff, and undo only hides messages with a marker until the next prompt. Compaction (`compaction.rs`) changes only the model's view; stored messages are never deleted.
- **Tool output** is bounded to 2000 lines / 50 KiB, with the remainder saved to a file. IDs are prefixed and time-ordered (`msg_`, `ses_`, `per_`, `evt_`).

## Pitfalls

- Nothing from upstream's runtime (Bun, Electron, current Node, Go 1.21+) loads on 10.14. No SwiftUI/Combine/async-await. Keep dependencies buildable with Xcode 11.3.1 and a 10.14 deployment target.
- Don't build upstream's HTTP/SSE client-server API, plugin system, LSP or event sourcing (see "Deliberately dropped" in `docs/10-roadmap.md`).
- Tests that find processes with `pgrep -f` must build the pattern from the process id (`uniq()`), or they match the command line that launched them.
- `docs/` quotes upstream prompts and tool descriptions verbatim. Keep `LICENSE-opencode` and `NOTICE` intact, keep the MIT notice in any file that copies substantial upstream text, and don't reuse upstream names or logos.
