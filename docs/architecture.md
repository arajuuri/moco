# Architecture of moco as built

The other documents are a usage reference (`usage.md`) and notes on supporting macOS 10.7 (`macos-10.7.md`). This one
describes the code as it is: the pieces, how a prompt travels through them, what is stored where, and which rules hold the design
together. When code and this document disagree, the code is right: fix the document.

Paths are relative to the repository root. Line counts are left out on purpose; they go stale.

## 1. Shape of the program

moco is one process with no server. A Cargo workspace has two crates:

```
crates/moco-core   the engine: no terminal, no UI types, no knowledge of how it is shown
crates/moco        the front end: argument parsing, the line-based chat, and the full-screen UI (tui/)
```

The only way the engine talks to a front end is the `Ui` trait (`engine.rs`). The front end calls
`Engine::run` and passes a `Ui`; the engine calls back with events, permission prompts, question forms and spending-limit
questions. There is no HTTP/SSE boundary, no plugin system and no LSP (moco does not reproduce them from opencode).

```
          crates/moco                               crates/moco-core
 +-----------------------------+      +------------------------------------------------+
 | main.rs                     |      | Engine (engine.rs, engine/*)                   |
 |   subcommands, flags,       | run  |   claim -> deliver -> step loop -> persist     |
 |   -p / --plain chat         |----->|     |         |            |                     |
 | tui/                        |      |     |         |            +--> tools::*  (scoped|
 |   mod.rs   event loop       |<-----|   Ui events,  |                 threads, 1/call)|
 |   app.rs   model + render   |  Ui  |   ask(), forms|                       |         |
 |   builtin  slash commands   |      |               v                       v         |
 +-----------------------------+      |        provider::* (one adapter   permission,   |
                                      |        per wire protocol)         sandbox,      |
                                      |               |                   classifier    |
                                      |               v                                 |
                                      |   Store (SQLite) . snapshots . blobs . log      |
                                      +------------------------------------------------+
```

Targets: macOS 10.7 through 10.14 on Intel, one binary built with Xcode 11.3.1, Rust 1.78 and a 10.7 deployment target
(`macos-10.7.md`). That rules out anything that needs a newer OS or async runtime
tooling, so the code is synchronous Rust on OS threads. HTTP is `ureq`, the database is bundled SQLite (`rusqlite`).

## 2. Modules of `moco-core`

| Area | Files | What it does |
|---|---|---|
| Engine | `engine.rs`, `engine/step.rs` | `Engine`, `EngineConfig`, `Ui`, `run`/`drain`, the step loop, request building, streaming and tool dispatch |
| | `engine/permit.rs` | `EnginePermitter`: rules, "always" answers, prompting, classifier hook |
| | `engine/classify.rs`, `classifier.rs` | Shell-command classifier (call, cache, verdict handling) and its prompt, parsing and never-classify list |
| | `engine/compact.rs`, `compaction.rs` | Manual and automatic compaction; what the model sees after a checkpoint |
| | `engine/undo.rs`, `snapshot.rs` | Per-step snapshots, undo/redo, session diffs |
| | `engine/jobs.rs` | Background shell commands and subagents |
| | `engine/limits.rs` | Step and cost limits |
| | `engine/title.rs`, `title.rs` | Title generation by a small model |
| Persistence | `store.rs` | SQLite schema, migrations, inbox, claims, saved permissions, usage, todos |
| | `chat.rs` | Stored messages to provider messages, with repair of dangling tool calls |
| | `blobs.rs`, `media.rs` | Content-addressed attachments; image and PDF recognition and limits |
| | `export.rs` | Markdown/JSON/JSONL export of a session |
| Providers | `provider/mod.rs` | Neutral `Request`, `Event`, `Block`, `Provider` trait, `Logged` wrapper |
| | `provider/{anthropic,openai_chat,openai_responses,gemini}.rs` | One adapter per wire protocol |
| | `provider/{registry,effort,retry}.rs`, `catalog.rs`, `auth.rs` | Which providers are connected, reasoning effort, backoff, model catalog, API keys |
| | `provider/{fake,testserver}.rs` | Test doubles |
| Tools | `tools/mod.rs` | `Tool`, `ToolCtx`, `Host`, `Permitter`, path helpers, the tool list |
| | `tools/*.rs` | `read write edit patch glob grep shell jobs webfetch websearch todowrite question subagent skill` |
| | `tools/shellparse.rs`, `tools/fsio.rs` | Command line to permission resources; atomic writes and per-file locks |
| | `nofollow.rs` | Opening, creating, renaming and removing files by a checked path without following symlinks |
| Safety | `permission.rs`, `wildcard.rs` | Rule model, modes, presets, evaluation |
| | `saved_permissions.rs` | Listing and removing saved "always allow" rules (`/permissions`, `moco permissions`) |
| | `sandbox.rs` | macOS Seatbelt wrapper for `shell` |
| Agents | `agent.rs` | Built-in agents, the system prompt, reminders |
| Extensions | `config.rs`, `instructions.rs`, `skills.rs`, `commands.rs`, `mentions.rs` | Config merge, `AGENTS.md`, skills, slash commands, `@` mentions |
| | `mcp/{client,http,oauth,manager,trust}.rs` | MCP over stdio and Streamable HTTP, OAuth, tool/prompt/resource exposure, project approval |
| | `format.rs`, `check.rs` | Formatter and check hooks run after edits |
| Misc | `log.rs`, `truncate.rs`, `id.rs`, `sysinfo.rs`, `markdown.rs` | Diagnostic log, output bounds, ids, host description, frontmatter |
| | `version.rs`, `describe.rs`, `build.rs` | The build's version from `git describe` (or `MOCO_GIT_DESCRIBE`), commit, target and minimum macOS |

## 3. The life of a prompt

This is the path every request takes, from keystroke to stored answer.

1. **Submit.** The front end calls `Engine::submit`. Mentions (`@file`, `@skill`, `@agent`) were already expanded by
   `mentions.rs` and are stored with the message. The prompt is written to `session_inbox` with a mode, `steer` (deliver at
   the next step boundary of a running turn) or `queue` (deliver when the session is idle). Nothing is lost if the process
   dies here: prompts are persisted before anything runs.
2. **Claim.** `Engine::run` calls `Store::claim`, which sets `session.claim_at` and `claim_pid` if no one holds the
   session. If a run already holds it, the result is `RunOutcome::Busy`. A `ClaimGuard` releases the claim on drop, so a panic
   cannot leave a session stuck. On startup `Store::recover_claims` finds claims whose process is dead and resumes those
   sessions (at most three times in a row).
3. **Drain.** `drain` loops. Each pass calls `Store::deliver`, which in one transaction moves inbox rows into the message
   table. New input resets the step allowance and the per-run cost allowance. With nothing delivered and nothing to continue,
   the run returns `Idle`. A title job runs alongside the first turn of an untitled session on its own thread and is cancelled
   shortly after the turn ends.
4. **Prepare** (`step.rs::prepare`). The history is reloaded from the store on every step. There is no in-memory
   conversation across steps. Auto-compaction reuses that loaded history and queries only newer messages (the
   checkpoint and any user shell results written while summarizing) before rebuilding the request.
   `compaction::effective` cuts it at the latest checkpoint,
   `chat::to_chat` rebuilds provider messages, and
   `agent::system_prompt` builds the system prompt (base text, tool guidance, agent text, `AGENTS.md`, skills, working
   directory, platform, date). The advertised tools are the built-in ones the agent may use plus the current MCP tools.
   Request-time reminders are added here and never stored: the step-limit notice, and the open todo list as `Request::tail`.
5. **Auto compaction.** If the request is larger than the model's threshold, the engine summarizes the older part first
   and prepares again.
6. **Snapshot and placeholder.** A working-tree snapshot is taken (best effort) and an empty assistant message is inserted.
   It is filled in as the step goes (tool calls are saved as they appear) and replaced by the final message at the end.
7. **Stream** (`step.rs::attempt`). `Provider::stream` pushes neutral `Event`s: text and reasoning deltas, tool calls,
   provider-private data, and a finish event with usage. Text goes to the UI as it arrives. A tool call is executed **the
   moment it is complete**, on a scoped thread, while the model keeps streaming. Tool-call parts are saved to the message as
   they appear. Retries (`provider/retry.rs`) happen only if no tool call ran in the failed attempt, and the partial output is
   discarded. A context-overflow error compacts once and retries the step.
8. **Finish.** Tool threads are joined, `AGENTS.md` files found by `read` are remembered, checks run once on the files the
   step changed, tool results are bounded (`truncate.rs`) and written into the message parts, usage and cost are added to the
   session, and the final assistant message is stored.
9. **Decide.** Tool calls present: `Continue` (another step, after the cost limits are checked). None: `Done`, and the loop
   delivers any queued input or returns `Idle`. A reply that stopped at the output token limit without a tool call is
   `CutOff`: `drain` stores a synthetic message asking the model to go on (or, if thinking used up the budget, to answer)
   and runs another step, at most twice in a row (archive#148). A refusal (`FinishReason::Refusal`, Anthropic's
   `stop_reason: "refusal"`) is `Failed` with a notice. It is never retried or continued, and the message is stored with
   `finish: "refusal"` so `chat.rs` leaves its text out of later requests (archive#153). Any other unknown stop reason ends the
   turn with a notice. `Declined`, `Interrupted`, `Failed` and `LimitReached` stop the run.

Cancellation is an `AtomicBool` passed down through the engine, the provider adapters (which check it at every read and
while waiting for headers), the shell tool (which kills the process group) and the retry sleep.

## 4. Storage

One SQLite file, `data/moco.db`, in WAL mode (several moco processes can read while one runs). Migrations are a numbered
list in `store.rs::MIGRATIONS`. The tables:

| Table | Holds |
|---|---|
| `project` | One row per directory, keyed by the symlink-free path |
| `session` | Directory, agent, title, model, variant, permission mode, parent (subagents), revert marker, cost and token totals, classifier cost, claim owner |
| `message` | `type` (`user`, `assistant`, `synthetic`, `compaction`) and a JSON `data` column |
| `session_inbox` | Prompts waiting for delivery, with `steer` or `queue` mode |
| `permission_saved` | "Always" answers per project |
| `todo` | The current todo list of a session |
| `instruction_state` | `AGENTS.md` files that `read` found, so later steps keep them |
| `auto_allow` | Commands the classifier allowed (pruned after seven days) |
| `meta` | Key/value bookkeeping, such as when the snapshots were last pruned (`claim_periodic`) |

Message shapes live in `chat.rs`. An assistant message is `{parts: [text | tool{id,name,input,state}], finish, usage,
cost, snapshot?, error?}`. A compaction message is `{summary, keepFrom, trigger}`. IDs come from `id::new_id(prefix)`: a
prefix, a strictly increasing time component and random characters, so sorting by id sorts by time (`msg_`, `ses_`, `per_`,
`evt_`).

Stored messages are never deleted by compaction or undo. Compaction changes only what the model sees. Undo marks messages
hidden through `session.revert` until the next prompt, which makes it permanent.

Files under `<app dir>` (`~/Library/Application Support/moco`, or `$MOCO_HOME`):

```
auth.json            API keys, mode 600
config/              user config, skills, commands, themes
cache/models.json    refreshed model catalog (an embedded snapshot is the fallback)
data/moco.db         sessions
data/snapshots/      content-addressed working-tree snapshots, per project, pruned after idle days
data/blobs/          attachments, by SHA-256
data/tool-output/    full text of truncated tool output and background job logs, pruned after a week
data/logs/           diagnostic log, one file a day
data/mcp-trust.json  approved project MCP servers, formatters and shells
data/tui-history     prompt history of the full-screen UI
```

Permissions (archive#164): the app dir, `data/` and every directory below it are 0700, and the files written there (and
`auth.json`, `mcp-auth.json`) are 0600, through `auth::create_private_dir`, `auth::write_private` (atomic
replace-by-rename) and `auth::create_private` (files written in place: logs, spill files, job logs). `cache/` holds only
the public catalog and keeps the umask default. The database file is created 0600 before SQLite opens it,
because SQLite gives `-wal` and `-shm` the main file's mode; the process umask is never changed, since shell commands
would inherit it. At session start `auth::secure_app_dir` tightens an older, looser `data/` and its known subdirectories
to 0700, and the app dir itself when it is the default location. A looser `$MOCO_HOME` is only warned about: the user may
share it on purpose. Session exports (`moco sessions export -o`, `/export`) are created 0600; an existing file keeps its mode.

## 5. Providers

`provider/mod.rs` defines a protocol-neutral request and event model:

- `Request` has the system prompt, `messages`, `tools`, `max_tokens` (`Engine::output_budget`: the catalog's `limit.output`
  capped by the `max_tokens` config, 16k when unknown, lowered by `fit_output` so prompt plus output fit the window),
  `tools_disabled` (keep the tool definitions for the
  cache but forbid calls), `effort`, an `overlay` from config, and `tail`.
- `Event` is `TextDelta`, `ReasoningDelta`, `ToolInputStart`, `ToolCall`, `BadToolCall` (input that was not valid JSON, so the
  model gets a tool error instead of the stream failing), `Native` and `Finish`.
- Provider-private data (OpenAI reasoning items, Gemini thought signatures, Anthropic thinking blocks) travels as
  `Block::Native` / `Event::Native`, is stored with the assistant message, and only its own adapter replays it.

One adapter per wire protocol: Anthropic Messages, OpenAI-compatible Chat Completions, OpenAI Responses and Gemini.
Responses is the default for the built-in `openai` provider (`ProviderInfo::protocol`, archive#185); Gemini has been
verified live. `provider/registry.rs` builds a client for every catalog
provider that has a key, wrapped in `Logged`. The catalog (`catalog.rs`) comes from models.dev and gives context limits,
prices and capabilities; config can add or override providers. A downloaded catalog cannot change the base URL, key
variables or protocol of a provider in the embedded snapshot (`Catalog::pin_builtins`). The shared HTTP agent
(`provider::new_agent`) follows no redirects, because ureq would carry custom key headers to the redirect target.

**Prompt caching is a design constraint.** Every request of a turn must extend the previous one byte for byte, because
`anthropic.rs` sets cache breakpoints on the last tool, the system prompt and the last messages, and a test
(`every_request_of_a_turn_extends_the_previous_one`) enforces it. That is why system prompt, tool schemas and tool-name
spelling are stable within a turn. Text that has to change from step to step goes in `Request::tail`, which adapters append
to the final user message after the breakpoints.

## 6. Tools and the permission pipeline

A tool implements `Tool` (`name`, `description`, `schema`, `run`). `run` gets a `ToolCtx`: working directory, project root,
spill directory, cancellation flag, a `Permitter`, and a `Host` for the few things that need engine state (todos, question
forms, subagents, background jobs). Tools never see the store or the UI directly. MCP tools implement the same trait and are
fetched from the manager on every request, which is why `Tool::name` returns `&str`.

Every tool that touches something asks first with `ctx.permit(PermReq)` or `ctx.check_external`:

```
tool -> PermReq{action, resources, save patterns, command?}
          |
          v  EnginePermitter::check
   rules = mode rules + preset + agent rules + user "permissions"   (flat list, last match wins, default ask)
          |
   deny (explicit rule) ----------------------------------------------> ToolError::Blocked
   allow ------------------------------------------------------------> run
   ask:
      shell command, only a *default* rule asks, auto mode, not opaque or a recognized file write
        and not on the never-classify list  --> classifier (small model) --> allow: run, with an "auto:" marker
                                                                         --> anything else: fall through
      Ui::ask (one prompt at a time, process-wide lock)
         once -> run      always -> save pattern to permission_saved, run
         reject -> stop the run          reject with text -> tool error carrying the feedback
```

Details that matter:

- **Modes.** `manual` (default) asks for everything except reads and planning tools. `auto` allows everything except `.env`
  reads, web search and unsandboxed shell. In `auto` mode the `careful` preset is added unless the config sets
  `permission_preset` (`trusted` adds nothing; an explicit `careful` applies to manual mode too).
- **Hard and soft asks.** A rule written by the user or by the careful preset's always-ask list (`git push`, `rm -rf`,
  `sudo`) is final. Only an ask that comes from a default rule (`Rule::soft`) may be relaxed by the classifier, and the
  classifier can only turn ask into allow. Timeouts, errors and garbled replies ask the user.
- **Paths** are compared with symlinks resolved (`tools/mod.rs::is_internal`), so a symlinked project is one project and a
  symlink that leads out of it is external. Anything outside the project asks under `external_directory`.
- **Check, then use the same file** (archive#165). The file tools resolve a path once (`real_path`), pass it to the checks
  (`ToolCtx::permit_path_at`, `check_external_file_at`) and then use exactly that symlink-free path through `nofollow.rs`,
  which refuses a symlink anywhere in it, so a link swapped in after the check (by a background shell, say) is not followed.
  With `openat` (Linux, macOS 10.10+, looked up with `dlsym` because 10.7 to 10.9 lack it) a folder is opened component by
  component with `O_NOFOLLOW` and the file is opened, created, renamed and removed relative to it. On 10.7 to 10.9 the same
  calls go by full path with `O_NOFOLLOW` on the last component, and the folder is checked again afterwards (no symlink, same
  device and inode); only a folder swapped out and back between two system calls slips past that. Opened files are `fstat`ed
  (a FIFO or device is refused without blocking). Tests swap links in at `nofollow::between_check_and_use`.
- **Shell** (`tools/shell.rs`) runs in its own process group so it can be killed whole. `shellparse.rs` splits a command line
  into permission resources and "always" patterns, and forces a prompt for opaque constructs (`eval`, `$(...)`, `base64 -d | sh`)
  and obvious file writes (in-place `sed`/`perl`, Python `open(..., 'w')`, and output redirections except `/dev/null`). With
  `sandbox.enabled` the command runs under `sandbox-exec` with a profile that denies writes outside the project, temp directories
  and `sandbox.writable`, keeps code-running paths inside them read-only (`.git/hooks`, `.git/config`, `.moco`, shell
  startup files, the cargo registry) and denies reading credentials (archive#123). `unsandboxed: true` asks under the `shell.unsandboxed` action. Waiting for a command (`wait_capped`,
  shared by foreground shells and background jobs) sleeps on a kqueue `NOTE_EXIT` watch on macOS, so it wakes when the command
  exits and otherwise every 100 ms to check `stop()` and the output cap; `try_reap` still decides whether the child exited. Without
  the watch (other platforms, or setup failed) it polls every 15 ms.
- **Output** is bounded to 2000 lines or 50 KiB; the rest is saved under `data/tool-output/` and the model is told the path.
- **Edits** (`edit`, `write`, `patch`) write atomically under a per-file lock (`fsio.rs`; the temporary file and the rename
  stay in the checked folder, opened through `nofollow.rs`), then run the configured
  formatter (`format.rs`). After a step that changed files, configured checks (`check.rs`) run once and their report is added
  to the tool result.

## 7. Agents, subagents and background jobs

Agents (`agent.rs`) are a prompt, a ruleset and a mode: `build` (everything) and `plan` (read-only, with the Plan reminder in
the system prompt) are selectable; `general` and `explore` are subagents. Config can add or change agents.

A **subagent** is an inline child session (`parent_id` set, depth at most 3) with the parent's directory and permission mode.
The `subagent` tool calls `Engine::run_subagent`, which submits the prompt to the child and runs it through the same `run`
path. The child's events reach the parent's `Ui` wrapped in `UiEvent::Nested`, its prompts go to the same user, and its cost
rolls up to its ancestors.

**Background work** (`engine/jobs.rs`) needs the engine to be shared: `Engine::share()` returns an `Arc<Engine>` and keeps a
weak handle so detached threads have an owner. A background shell command writes to a file under `data/tool-output/`; a
background subagent runs on its own thread with an `Unattended` `Ui` that refuses prompts (the classifier may still answer,
if its `background` setting allows). When a job ends, its result is queued as a `synthetic` steer message for the owning
session and the front end is told through the engine's notifier.

## 8. Keeping history under control

- **Compaction** (`compaction.rs`, `engine/compact.rs`). A `compaction` message stores a summary and the id of the first
  message to keep. The summary is written by the same model, with a prompt of moco's own. `effective()` returns
  `[summary, kept messages...]`, and a later compaction summarizes the previous summary plus what followed, so there is
  at most one checkpoint. It runs on `/compact`, when a request nears the model's limit, and once after a provider's
  context-overflow error.
- **Snapshots** (`snapshot.rs`) are `git`-free: a manifest of every non-ignored file with its SHA-256 and mode, contents stored
  once as blobs, only changed files re-hashed. Symlinks are not followed or recorded; a file the walk saw is read through
  `nofollow.rs`, so a link swapped in before it is read is skipped as unreadable (archive#165). One is taken before every step and its id is stored on the assistant message.
  `/undo` restores files and hides the messages; `/diff` compares the current tree with the session's first snapshot.
  `.gitignore` files apply at every level (also in repositories below a non-git session directory), and `node_modules`
  and directories tagged with `CACHEDIR.TAG` (Cargo's `target/`) are never entered. A capture first only stats the tree
  and checks its budget (50,000 files, 512 MiB of files to store; `Snapshots::max_files`/`max_total_bytes`), so a
  directory over it fails before any file is read or stored (#23); the step runs without a snapshot and snapshots are off
  for that session (in this process) with one notice. Files over 2 MiB are not stored and are reported as skipped.
  Interrupting the run cancels a capture in progress.
- **Limits** (`engine/limits.rs`). A step allowance per run (the last allowed step forbids tools and adds the `MAX_STEPS`
  reminder), and cost limits per run and per session checked after each step that is followed by another. An attended UI is
  asked what to do, an unattended one or `-p` stops the run.

## 9. Extensibility

- **Config** (`config.rs`): `moco.json[c]` files from the user directory, each ancestor of the working directory and
  `$MOCO_CONFIG`, merged lowest to highest priority, with `{env:NAME}` and `{file:path}` substitution (`{file:}` reads
  only a regular file of at most 1 MiB, opened non-blocking and checked with fstat, so a device or FIFO cannot hang or
  exhaust startup; a refused token drops that config file with a warning naming it). Objects merge by key,
  `permissions` and `instructions` arrays concatenate, everything else is replaced. Some settings are read **only** from the
  user's own files: a provider's `base_url`, `env`, `protocol` and `headers`, and `permission_classifier`. A project config
  that sets them is ignored with a warning, and a project's `permissions` can only add `deny` rules.
- **Anything a project config can make moco run** (MCP servers, formatters, checks, the shell executable) must go through
  the approval in `mcp/trust.rs`, remembered per project directory and exact definition.
- **Instructions, skills, commands** share one directory list, `skills::config_roots` (`~/.claude`, `~/.agents`, the app config,
  then `.claude`, `.agents`, `.moco` of each ancestor). `AGENTS.md` (with `CLAUDE.md` as fallback) is rendered into the system
  prompt.
- **MCP** (`mcp/`): `client.rs` (stdio) and `http.rs` (Streamable HTTP, with `oauth.rs`) implement one `McpConn` trait.
  `manager.rs` runs the servers and exposes tools as `<server>_<tool>`, prompts as slash commands and resources through an
  `mcp-resource` tool and `@` mentions. The HTTP and OAuth paths have been checked only against fake servers.

## 10. Front ends

**`main.rs`** parses arguments, handles the subcommands that need no session (`auth`, `sessions`, `logs`, `models`), then
loads config, applies the sandbox setting, opens the store, recovers crashed runs, builds the provider map, the MCP manager and
the `Engine`, and picks a mode:

| Mode | When | What it does |
|---|---|---|
| One-shot | `-p "prompt"` | One run, exit status 0 done, 1 failed, 2 usage, 3 stopped by a limit |
| Line chat | `--plain`, or stdin/stdout is not a terminal | Reads lines, prints streamed text, tool lines and checklists |
| Full screen | A terminal (default) | `tui::run` |

**The full-screen UI** (`crates/moco/src/tui/`) separates state from effects:

- `app.rs` is a pure model. `App::update(AppEvent) -> Action` and `render(Frame, &mut App)` touch no terminal and no engine,
  so tests drive them with scripted events and read ratatui's `TestBackend`.
- `mod.rs` is the event loop. The UI thread owns the terminal. An input thread turns crossterm events into `AppEvent`s. It is
  not scoped, so leaving the UI never waits for it: crossterm 0.27 spun forever on a hung-up tty (archive#87). Until that behavior
  is verified with crossterm 0.28 on Lion, before each poll the thread checks the tty with `term::tty_gone` (`select` plus
  `FIONREAD`; macOS `poll` does not work on ttys) and, when the terminal is gone, ends the UI the way SIGHUP and SIGTERM do
  (`SIGNALLED`). The loop draws only when the screen can have changed: any event but `Tick`, a `Tick` while `App::animating()`
  (a running turn, jobs, a pending retry, a running tool row), or a `sync_queue` that changed something. `render` must not depend
  on `tick` for anything `animating()` does not cover. The
  engine runs on a worker thread per run (`start_run`), inside a `thread::scope`. The worker's `TuiUi` converts each borrowed
  `UiEvent` to an owned `Ev` and sends it down the channel. `Ui::ask` sends the prompt with a reply channel and blocks the engine
  thread until the UI thread answers. While a turn runs, or jobs or the sidebar are active, the loop also syncs the inbox and the sidebar (MCP
  servers, jobs) about every 200 ms. Background-job lines arrive through the engine's notifier.
- After a turn ends normally (`RunDone(Idle)`), the app returns `Action::Suggest`, and the loop asks
  `Engine::suggest_next` (`engine/suggest.rs`, prompt in `suggest.rs`) on a scoped thread for one short next message
  from the small model. It arrives as `AppEvent::Suggestion` tagged with `App::suggest_turn`, so one for an older turn
  (a new run, `/new`, `/resume`) is dropped. It is drawn as ghost text in an empty composer; Tab copies it into the input,
  any input dismisses it, and it is never sent by itself. `tui.suggestions: false` turns it off.
- `builtin.rs` implements the slash commands, `keys.rs` the key map, `theme.rs` the colors, `md.rs` Markdown rendering,
  `diff.rs` the diff viewer, `complete.rs` completion.
- When a session is opened (start, `/resume`, `/new`, `/fork`), `load_session_view` rebuilds the transcript, costs and todo
  list from the store.
- A panic on the worker thread is caught and reported as a failed run, so the UI never waits for a run that is gone.

## 11. Observability

`log.rs` writes a diagnostic log, never to the terminal: `MOCO_LOG` or the `log` config key sets the level (default `warn`,
per-target levels such as `warn,mcp=trace`), one file a day under `data/logs/`, seven days, 16 MiB a day shared by all moco
processes, with credentials redacted. Targets are `provider`, `engine`, `tool`, `shell`, `jobs`, `mcp`, `store`, `snapshot`,
`classifier`, `main` and `log`. `moco logs` shows the tail. Failures that would otherwise be silent (store writes, saved
permissions) become a one-time notice in the UI through `notice_once`.

## 12. Testing

`cargo test` needs no network. The engine is tested with a scripted fake provider (`provider/fake.rs`) that replays events and
records every request, and with local fake servers (`provider/testserver.rs`). Integration tests under `crates/moco/tests/`
drive the real binary against a Python fake LLM server (`fake_llm.py`) and, for the TUI, through a pty (`tui_pty.py`);
the MCP tests spawn `fake_mcp*.py`. The classifier has a golden set (`crates/moco-core/tests/classifier_cases.jsonl`) checked
offline for the rules that need no model, and `scripts/eval-classifier.py` measures the model layer against a live provider.

## 13. Rules the design depends on

These are the invariants that are easy to break by accident.

1. **The engine knows nothing about a UI.** New information for a front end becomes a `UiEvent` variant, not a dependency.
2. **History is reloaded from the store on every step**, and prompts are persisted before they run. State kept only in memory
   is lost on a crash, so do not rely on it.
3. **Requests keep a byte-stable prefix within a turn.** Anything that changes mid-turn goes in `Request::tail`, never in the
   system prompt, the tools or earlier messages.
4. **Retry only when no tool ran** in the failed attempt, and discard the partial output.
5. **The classifier can only turn an ask into an allow**, never override a rule, and never sees opaque, dangerous, or obvious shell file-writing commands.
6. **Project config is untrusted.** Anything it can make moco execute goes through the approval; credentials and endpoints are
   read only from the user's own config.
7. **Tool output is bounded** before the model sees it, and the rest is saved to a file.
8. **Stored messages are never deleted** by compaction; undo hides them until the next prompt.
9. **No runtime that 10.7 cannot load:** no Bun, Electron or current Node, no async-await toolchain, no dependency that needs a
   newer Xcode than 11.3.1 or Rust than 1.78, and no symbol newer than 10.7.
