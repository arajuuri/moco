# Using moco

moco is a coding agent for the terminal. This is the reference for the app as built; the other documents describe the design. Phases 0-2 are implemented (see `10-roadmap.md` for results and what is left).

## Install

On the Mac (Intel; building needs Mojave or another 10.12+ release, and the binary runs on 10.7 and newer), from a checkout of this repository:

```
scripts/install.sh --add-to-path
```
It checks the system, builds a release binary for macOS 10.7 and newer into `~/.cache/moco/target` (the Xcode command line tools must be installed: `xcode-select --install`; if Rust is missing it offers to install it with rustup), verifies the binary's minimum macOS version, installs it to `~/.local/bin/moco` and, with `--add-to-path`, adds that directory to `~/.bash_profile` (or `~/.zprofile` for zsh) once. Open a new terminal afterwards, or `export PATH="$HOME/.local/bin:$PATH"`. Other options: `--prefix /usr/local` (installs to `/usr/local/bin`; you need write access), `--target-dir DIR` for the build cache, `--yes` to never ask. Running it again after a `git pull` updates the install. `scripts/install.sh --uninstall` removes the binary and keeps your sessions and keys (`--purge` removes those too). `moco --version` prints the version (for example `0.1.0-12-g1a2b3c4d5e`: 12 commits after release 0.1.0, at that commit; see "Versions and releases" in `CONTRIBUTING.md`), the commit and the minimum macOS.

By hand: `MACOSX_DEPLOYMENT_TARGET=10.7 CARGO_TARGET_DIR=<a directory outside the repo> cargo build --release --locked`, then check `otool -l moco | grep -A3 LC_VERSION_MIN_MACOSX` (must say 10.7). On Lion, rustup and `scripts/install.sh` don't work; build on a newer Mac and copy the binary over, or build with a MacPorts Rust toolchain (`docs/14-macos-10.7.md`). The binary needs no runtime besides the system libraries.

## First run

1. Give moco an API key: `moco auth anthropic` (reads the key from stdin, echo off) or set `ANTHROPIC_API_KEY`. Keys live in `~/Library/Application Support/moco/auth.json` (mode 600). Other providers: `moco auth openrouter`, or the env var shown by `moco models`. Shell commands, stdio MCP servers, formatters and checks do not inherit provider-key environment variables by default; pass one deliberately with an inline assignment such as `ANTHROPIC_API_KEY=... cmd`, or for a server, formatter or check in its `environment` config.
2. `cd` into a project and run `moco`. Type a request; the answer streams, tool calls show as dim `>` lines, and a line after each turn shows tokens, cost, session cost and how full the context is.

## Command line

| Command | What it does |
|---|---|
| `moco` | Interactive chat in the current directory (new session) |
| `moco -p "prompt"` | One request, then exit (non-zero if it did not finish) |
| `moco --continue` / `-c` | Resume the latest non-archived session of this directory |
| `moco --session ID` / `-s` | Resume any session by a piece of its id (it runs in its own directory) |
| `moco --model provider/model` / `-m` | Model for this session (also `MOCO_MODEL`) |
| `moco --agent ID` | Agent for this session (`build` default, `plan` read-only); only primary agents can be chosen |
| `moco --permission-mode manual\|auto` | Mode for this session: ask before acting (`manual`, the default) or run tools without asking (`auto`); also the default for new sessions, overriding `permission_mode` in config |
| `moco --dir PATH` | Work in another directory |
| `moco models [filter] [--refresh]` | Providers with connection state, or models with context size and price; `--refresh` downloads the full catalog |
| `moco auth PROVIDER` | Store an API key |
| `moco sessions [--all] [--archived]` | List sessions; `rename ID TITLE`, `archive ID`, `unarchive ID`; `export ID [--md\|--json\|--jsonl] [--children] [--include-hidden] [-o FILE]` writes a transcript (Markdown by default, to stdout without `-o`; read-only, safe while the session runs). Exports are **not redacted**: pasted keys and tool output appear as stored |
| `moco logs [--follow] [--lines N] [--path]` | Show the end of the newest log file, follow it, or print the log directory (see "Logging") |
| `moco permissions [--dir DIR]` | List the "always allow" rules saved for this directory's project (or `DIR`'s), numbered; `remove N`, `remove ACTION PATTERN` (quote the pattern: `remove shell 'osascript *'`) or `clear` forgets one or all of them. Works while a session runs; that session stops using a removed rule from its next step |

## Inside the chat

| Command | |
|---|---|
| `/model [filter \| provider/model]` | Show, search or switch the model for this session |
| `/agent <id>` | Switch agent |
| `/mode [manual\|auto]` | Show or switch the permission mode (Shift-Tab does the same in the full-screen chat) |
| `/compact` | Summarize older messages to free up context (also automatic near the limit) |
| `/undo`, `/redo` | Undo the last turn's file changes and messages (repeat to go back further) or bring them back; the next prompt makes an undo permanent |
| `/diff` | Changes in the working tree since the session started |
| `/sessions [all\|archived]`, `/resume <n\|id>` | List and continue sessions (numbers refer to the last list) |
| `/export [path]` | Write this conversation as Markdown (default `moco-<id>.md` in the project directory; not redacted) |
| `/rename <title>`, `/fork [N]`, `/archive [n\|id]`, `/unarchive <n\|id>` | Manage sessions; `/fork N` branches off before your Nth most recent prompt |
| `/skills`, `/commands` | List the skills and custom slash commands that were found (MCP prompts included) |
| `/mcp`, `/mcp reconnect <name>` | Show the MCP servers and their state, or restart one |
| `/mcp auth <name>`, `/mcp logout <name>` | Sign in to a remote MCP server (OAuth), or forget its stored sign-in |
| `/jobs`, `/jobs output <id>`, `/jobs kill <id>` | Background commands and subagents: list, read output so far, stop |
| `/permissions` | What the classifier allowed automatically in this session, and the numbered "always allow" rules saved for the project |
| `/permissions remove <n>`, `/permissions remove <action> <pattern>`, `/permissions clear` | Forget one saved "always allow" rule (by its number in `/permissions`, or e.g. `remove shell osascript *`), or all of them for the project; the next matching request asks again |
| `!<command>` | Run a shell command yourself, right now; the assistant is told what you ran |
| (empty line) | Let the assistant react to a background result that arrived while you were idle |
| `/<name> [args]` | Run a custom command (built in: `/init`, `/review`) |
| `/new`, `/help`, `/exit` | |

`Ctrl-C` interrupts a running turn (running commands are killed); `Ctrl-D` at the prompt quits.

The status line shows the session's permission mode (`manual`, or `auto` in a warning colour). **Permission prompts** show the action and what it touches: `y` allow once, `a` allow always (saved for this project with a pattern such as `git push *`; `/permissions` lists these and `/permissions remove <n>` forgets one), `n` reject and stop, or type feedback to reject and tell the model why. In the default `manual` mode, anything that changes files, runs a command, or fetches from the web asks; reads and searches do not. In `auto` mode everything is allowed except reading `.env` files and web searches. Your own rules (below) apply in both modes. The `question` tool shows a numbered form; Enter dismisses it.

## Other providers and protocols

Anthropic and OpenAI-compatible Chat Completions servers are the well-trodden paths. Two more protocols are **experimental** (written from the documentation, not yet tried against the live services, labelled `[experimental protocol]` in `moco models`):
- **Google Gemini**: `moco auth google` (or `GEMINI_API_KEY`), then `--model google/gemini-2.5-flash`.
- **OpenAI Responses**: opt in with `{ "providers": { "openai": { "protocol": "openai-responses" } } }`. Reasoning models then keep their reasoning between turns.
If one misbehaves, `/model` back to another provider and report the error text.

## Skills, commands and mentions

- **Skills** are folders with a `SKILL.md` (frontmatter `name` and `description`, then instructions) in `.moco/skills/`, `.claude/skills/` or `.agents/skills/` of the project or any parent directory, or in `~/Library/Application Support/moco/config/skills/` and `~/.claude/skills/`. The model sees the list and loads a matching skill itself with the `skill` tool; you can also load one with `@skill-id`.
- **Custom commands** are markdown files in `commands/` next to those folders (or `commands` in the config). The file name is the command, the body is the prompt; `$ARGUMENTS` is replaced by what you type after the name. Optional frontmatter: `description`, `agent`, `model` (for that prompt only) and `subagent: true` (run in a child session and report back).
- **Mentions**: `@src/main.rs` attaches that file, `@docs/` a directory listing, `@release` a skill, `@explore` asks the model to delegate to that subagent. What was attached is shown as a dim note.
- **Images and PDFs**: `@screenshot.png` or `@spec.pdf` (or dragging the file into the terminal, which pastes its path) sends the file to the model; the model can also open them itself with `read`. Images up to 5 MiB and 8000 px per side, PDFs up to 10 MiB, and only for models that accept them (`moco models` shows the catalog; a text-only model gets a note instead).

## Full-screen UI

On a terminal `moco` runs the chat full screen (works over SSH). `moco --plain` gives the line-based chat described below (it is also used with `-p` and when output is piped). Enter sends, Esc or Ctrl-C interrupts a running turn (Ctrl-C on an empty prompt, or Ctrl-D, quits), PageUp/PageDown scroll, Ctrl-End jumps back to the newest output. A permission request replaces the prompt box: `y` once, `a` always, `n` reject, `f` reject with feedback. Alt-Enter (or Ctrl-J) adds a line, Up/Down move through lines and then your prompt history, Tab completes `/commands` and `@files`, `@skills`, `@agents`, Shift-Tab switches the permission mode; big pastes collapse to a placeholder; `!command` runs a shell command. `Ctrl-P` opens a command palette. The built-in commands (`/model`, `/agent`, `/sessions`, `/undo`, `/diff`, `/compact`, ...), custom commands, MCP prompts and the assistant's question forms work. While a turn runs, Enter steers your text into it (delivered at the next step), Ctrl-Q or `/queue <text>` queues it for after the turn, and waiting prompts are listed above the box (`/queue` manages them); After a turn ends, the empty message box shows a **suggested next message** in dim italics (for example "Run the tests"), written by the small model from your last prompt, the reply and open to-dos: Tab takes it as typed text that you can edit and send with Enter, and typing anything else replaces it. It is never sent on its own. Each suggestion is one small-model call, added to the session cost; `"tui": {"suggestions": false}` turns them off. Ctrl-O cycles how much is shown: normal (inline diffs of file changes, reasoning summarized as one `thinking (N lines, Ctrl-O shows all)` line), full (all reasoning, tool output, every subagent call) and compact (call lines only). The hints name the key `detail` is bound to. When a turn ends with reasoning but no answer (for example it stopped at the output token limit, failed or was interrupted), its last reasoning is shown in full at the normal level, since it is all the model produced. Replies are rendered as Markdown. `/diff` opens a full-screen viewer of the session's file changes (`j`/`k`, Space/`b`, `n`/`p` for the next or previous file, `q` closes). Not available yet: `/mcp auth` and subagent-style custom commands; use `moco --plain` for those.

### Full-screen UI settings

```jsonc
{ "tui": {
    "theme": "auto",                       // auto, dark, light, mono, or a theme file name
    "keys": { "palette": "ctrl+k", "quit": ["ctrl+d", "ctrl+q"], "sidebar": "f3" },
    "leader": { "n": "/new", "g": "/diff" },   // Ctrl-X then a key
    "bell": true, "away_after": 8,          // bell + window title when a turn ends or input is needed while you are away
    "sidebar": "auto",                     // true, false, or auto (from 110 columns)
    "ascii": false,                        // true: draw with plain ASCII only (also MOCO_ASCII=1)
    "suggestions": true } }                // suggested next message after each turn (one small-model call each)
```
Actions you can rebind: `interrupt` (Esc), `quit` (Ctrl-D), `palette` (Ctrl-P), `newline` (Alt-Enter, Ctrl-J), `queue` (Ctrl-Q), `detail` (Ctrl-O), `effort` (Ctrl-T), `sidebar`, `page_up`, `page_down`, `top`, `bottom`, `leader` (Ctrl-X). Keys are written `ctrl+k`, `alt+enter`, `pageup`, `f2`, `x`; `null` unbinds. The leader menu (type Ctrl-X to see it) has n new, l sessions, m model, a agent, d diff, c compact, u undo, r redo, j jobs, t theme, h help, q exit, b sidebar, o detail, e effort. Themes: `/theme` picks one; a theme file is JSON in `~/Library/Application Support/moco/config/themes/NAME.json` (or `.moco/themes/` in a project), for example `{ "extends": "dark", "user": "#00afff bold", "bar": "white on 238" }` with the roles user, text, dim, error, accent, bar, bar_hi, code, heading, quote, link, added, removed, panel, border, ok, warn. `#rrggbb` colors become the nearest of the 256 colors on terminals without truecolor, like Terminal.app.

## MCP servers

MCP servers add tools (and prompts and resources) from other programs. Declare them in the config:

```jsonc
{ "mcp": { "servers": {
    "files": { "command": ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/Users/me/Documents"] },
    "db":    { "command": ["/usr/local/bin/my-db-server"], "environment": { "DB_URL": "{env:DB_URL}" } } } } }
```
- Their tools are named `<server>_<tool>` and are checked like any other tool: add a rule such as `{ "action": "files_*", "resource": "*", "effect": "ask" }` to be asked first. Prompts are slash commands `/<server>:<prompt> args`; resources can be mentioned with `@server:name`.
- **Servers declared in a project's own config file ask for your approval** before starting (a terminal is required; "always" is remembered for that project and that exact definition; `MOCO_TRUST_PROJECT_CONFIG=1` approves them without asking). Servers in your user config (`~/Library/Application Support/moco/config/`) start without asking.
- `/mcp` shows what is running; a server that fails to start prints its reason and stderr at startup.
- **Remote servers** (MCP Streamable HTTP): `"docs": { "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer {env:TOKEN}" } }`. A server that answers 401 shows `needs authorization`; run `/mcp auth <name>` to sign in. moco prints an address to open in a browser (and tries `open`/`xdg-open`) and listens on a loopback port for the redirect. Over SSH, open the address on your own machine and paste the address the browser ended up on at the prompt. Tokens are stored in `mcp-auth.json` in the app directory (mode 600, never in the Keychain) and refreshed automatically; `/mcp logout <name>` removes them. `"oauth": { "client_id", "client_secret", "scope", "callback_port", "redirect_uri", "auth_server_metadata_url" }` pre-configures a client, `"oauth": false` disables sign-in. moco follows a server's redirects only on the same host, so your headers and tokens are not sent anywhere else; if a server moved to another host, put its new address in `url`.
- Remote HTTP and OAuth support is experimental: it has been verified against local fake servers and a live model on Mojave, not against real hosted MCP services.

## Reasoning effort

Models that can think more or less offer *levels*: `/effort` shows the ones the current model has and `/effort high` (or `/effort default`) sets it for the session; `moco --effort high` sets it at start; in the full-screen UI Ctrl-T steps through the levels and the status bar shows `think high`. Leader `e` opens the level list. A level the model does not have is ignored, so you can switch models freely. Defaults come from the config:

```jsonc
{ "effort": "medium",                                   // for every session
  "agents": { "plan": { "effort": "high" } },           // per agent
  "providers": { "myserver": { "models": { "big": {
      "request":  { "headers": { "x-team": "me" } },    // sent with every request to this model
      "variants": { "deep": { "body": { "reasoning": { "effort": "high" } } } } } } } } }  // a level of your own
```
Anthropic's current models take `low` to `max` (and `none` where thinking can be switched off); OpenAI's reasoning models `minimal` to `high`; Gemini a budget. Reasoning is shown in the transcript as one summary line (Ctrl-O for the full text; a turn that ends without an answer shows its last reasoning in full). More about how each provider is asked: `docs/07`.

## Web search

The assistant can search the web with the `websearch` tool and read pages with `webfetch`. By default it uses DuckDuckGo, which needs no account or key. **moco asks before every new kind of search** (your words are sent to the search service); answer `a` to allow searches for the project, or add `{ "action": "websearch", "resource": "*", "effect": "allow" }` to `permissions` in the config. A fetch from this machine or its network (a host that is or resolves to a loopback, private, link-local or similar address, such as `localhost`, `192.168.x.x` or the cloud metadata address `169.254.169.254`, also when a redirect leads there) asks for `webfetch.private` in every mode; `a` allows that origin for the project.

```jsonc
{ "websearch": { "provider": "brave", "max_results": 8 } }   // duckduckgo (default), brave, tavily, exa, searxng
{ "websearch": { "provider": "searxng", "url": "https://searx.example.org" } }
{ "websearch": false }                                        // no web search at all
```
Brave, Tavily and Exa need an API key: set `BRAVE_API_KEY` (`TAVILY_API_KEY`, `EXA_API_KEY`) or run `moco auth brave`. DuckDuckGo may show a captcha after many searches in a short time; moco then says so and you can switch provider.

## Formatters

moco can run a formatter on every file the model writes, edits or patches. Nothing runs unless you configure it:

```jsonc
{ "formatter": {
    "rustfmt":  { "command": ["rustfmt", "--edition", "2021", "$FILE"], "extensions": [".rs"] },
    "prettier": { "command": ["npx", "prettier", "--write", "$FILE"], "extensions": [".js", ".ts", ".json"] } } }
```
If the formatter changes the file, the model is told to re-read it before editing further; if the formatter fails, the model is told why and the file stays as written. As with MCP servers, a formatter defined in a *project's* config asks for your approval first. `"formatter": false` turns formatting off.

## Background work

The model can run a long command in the background (`shell` with `background: true`: a dev server, a long build or test run) or start a subagent in the background, and carry on with other things. It is told when the job ends, together with the result, so it does not poll. While a turn is running that happens at its next step. While the TUI is idle you see a dim `[background ...]` line and the assistant reacts to it at once (after you close a picker, viewer or dialog you have open; text you are typing is left alone). `"jobs": {"auto_resume": false}` turns that off; then, as always in the line-based chat (`--plain`), the assistant reacts with your next message or when you press Enter on an empty line. `moco -p` waits for background jobs after the turn and lets the assistant report. Background subagents cannot ask you for permission (anything that would ask is refused). Everything still running is stopped when moco exits.

## Tools the model has

`read`, `write`, `edit`, `patch`, `glob`, `grep`, `shell`, `jobs`, `webfetch`, `todowrite`, `question`, `subagent` (agents `general` and `explore`), `skill` when skills exist, and `<server>_<tool>` for each tool of a configured MCP server. Paths outside the project ask for an `external_directory` permission in `manual` mode and are allowed in `auto` mode. Long output is cut to 2000 lines or 50 KiB and the full text is saved under `data/tool-output/`.

## Configuration

Files named `moco.json` or `moco.jsonc` (comments and trailing commas allowed), merged from lowest to highest priority: `~/Library/Application Support/moco/config/`, then every parent directory of the working directory (and its `.moco/` subdirectory) outermost first, then `$MOCO_CONFIG`. `{env:NAME}` and `{file:path}` are substituted in the text of your own config files (not in a project's, so a cloned repository cannot read your keys or files into a URL or header it chooses). A project's config also cannot change `websearch` (except `false` and `max_results`), provider endpoints, keys, headers or request overlays, or name `instructions` files outside the project; such keys are ignored with a warning. Objects merge by key; `permissions` and `instructions` lists add up.

```jsonc
{
  "model": "anthropic/claude-sonnet-5-5",
  "small_model": "anthropic/claude-haiku-5-5",          // titles
  "default_agent": "build",
  "permission_mode": "manual",                           // default for new sessions (a project's config may only say manual): see docs/05-permissions.md
  "shell": "/bin/zsh",
  "permissions": [                                       // last matching rule wins
    { "action": "shell", "resource": "ls *", "effect": "allow" },
    { "action": "edit",  "resource": "*.lock", "effect": "deny" }
  ],
  "agents": { "docs": { "description": "Writes docs", "system": "Be brief.", "permissions": [] } },
  "instructions": ["docs/STYLE.md"],                     // extra files like AGENTS.md
  "providers": { "ollama": { "base_url": "http://localhost:11434/v1",
                 "models": { "qwen3-coder": { "limit": { "context": 32768, "output": 8192 } } } } },
  "compaction": { "auto": true, "buffer": "15%", "keep": 20000 },
  "max_tokens": 32000,                                   // cap on the output budget per request (thinking included); see below
  "snapshots": true,
  "skills": ["~/shared-skills"],                         // extra skill directories
  "commands": { "ship": { "description": "Tag and push", "template": "Tag the release and push it. $ARGUMENTS", "agent": "build" } }
}
```
Actions are `read`, `edit` (write, edit and patch), `glob`, `grep`, `shell`, `webfetch`, `question`, `subagent`, `external_directory`. Details: `05-permissions.md`, `08-config-and-extensions.md`, `07-providers.md`.

**Output budget**: each request asks for the model's own output limit (`limit.output` in the catalog, or set per model as in the `ollama` example), 16,000 when the limit is unknown. `max_tokens` caps it (and is the budget for models without a limit). Only the tokens written are billed. Thinking counts against it. A reply cut off at the budget is continued automatically, up to twice in a row.

**Project instructions**: `AGENTS.md` (or `CLAUDE.md` where there is no `AGENTS.md`) in the project root down to the working directory, `config/AGENTS.md` globally, plus `instructions` files. They are re-read every step. A project file (including a relative `instructions` entry) that is a symlink, or sits under one, resolving outside the project is not read; moco shows a notice naming it instead. Absolute and `~/` entries from your own config may point anywhere.

## Logging

moco writes a diagnostic log, never to the terminal. `MOCO_LOG=error|warn|info|debug|trace` (or the `log` config key; the variable wins) sets the level, `warn` by default. `warn,mcp=trace` sets one target; the targets are `provider`, `engine`, `tool`, `shell`, `jobs`, `mcp`, `store`, `snapshot`, `classifier`, `main` and `log`. Files are `data/logs/moco-YYYY-MM-DD.log` (mode 600), one per day, seven days kept, 16 MiB at most per day file (the cap is shared by all moco processes). The first line says which moco, OS and kernel wrote it. `moco logs` shows the end of the newest file.

Keys, `Authorization`-style headers, cookies, URL tokens and credentials on command lines (`TOKEN=...`, `curl -u user:pass`, `--password`, connection strings) are replaced by `[redacted]` before a line is written. Request and response bodies are logged at `trace` only.

The `classifier` target is on at `info` whatever the default level is, so every automatic permission verdict is on record, with the command (redacted, shortened). Turn it off with `MOCO_LOG=warn,classifier=off`. Commands the classifier allowed are also kept in the database for `/permissions` for seven days, redacted the same way. Commands, tokens and paths that do not look like credentials are not recognized, so treat the log as sensitive.

Only a run that starts a session creates a log: `--help`, `--version`, `logs`, `models`, `auth` and `sessions` leave no file behind.

## Where things are stored

Everything is under `~/Library/Application Support/moco/` (`MOCO_HOME` overrides): `auth.json` (keys), `config/`, `cache/models.json` (refreshed catalog), `data/moco.db` (sessions, SQLite), `data/snapshots/` (undo data, pruned after 30 idle days), `data/tool-output/`, `data/logs/` (diagnostic log, 7 days). Nothing is sent anywhere except to the model provider you chose and the URLs the model fetches.

## Environment variables

`ANTHROPIC_API_KEY` and the other provider keys, `MOCO_HOME`, `MOCO_MODEL`, `MOCO_CONFIG`, `MOCO_SHELL`, `MOCO_LOG`, `ANTHROPIC_BASE_URL`.
