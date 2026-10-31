# Config, instructions, skills, commands, MCP

Sources: `schema/src/config.ts` and `config/*.ts`, `core/src/config.ts`, `config/discovery.ts`, `config/markdown.ts`, `config/plugin/instruction.ts`, `skill.ts`, `skill/*.ts`, `mcp/*.ts`, `schema/src/mcp.ts`.

## Config files

Names: `opencode.json` or `opencode.jsonc` (JSONC with trailing commas allowed; `jsonc-parser`). Rename for your app, but also consider reading the upstream names so users can reuse config.
Locations, roughly lowest to highest priority (the code documents `entries()` as lowest to highest; the exact interleaving is in `config/discovery.ts`, which I only skimmed, so read it before fixing precedence):
1. System: `/Library/Application Support/<app>/` on macOS; managed preferences plist `ai.opencode.managed` under `/Library/Managed Preferences/` (MDM; skip unless needed)
2. Remote "well-known" config from a configured domain (skip)
3. Global: `<config dir>/opencode.json[c]`, plus `~/.claude` and `~/.agents` compatibility dirs
4. Project: for every ancestor of the working dir, look for `.claude/`, `.agents/`, `.opencode/` directories and `opencode.json[c]` files (skipped entirely when the working dir is itself a global config dir)
5. Explicit file/content passed on the command line
Variable substitution is applied to the raw config text before parsing (`config/variable.ts`): `{env:NAME}` (empty string if unset; JSON-escaped if the raw value would break the JSON) and `{file:path}` (file contents; read `substituteFiles` for path resolution rules).
Settings merge by key; arrays like `permissions` concatenate in priority order. Watch config files and reload live (`config.updated`).

## moco implementation notes (Phase 2, config slice)

Implemented in `crates/moco-core/src/config.rs` and `instructions.rs`.
- File names are `moco.json` / `moco.jsonc` (upstream's `opencode.json` is not read). Locations, lowest priority first: `<app dir>/config/`; then each ancestor of the working directory, outermost first, as `<dir>/moco.json[c]` and `<dir>/.moco/moco.json[c]`; then the file named by `$MOCO_CONFIG`. A file that fails to parse is skipped with a warning.
- Merge: objects merge by key, `permissions` and `instructions` arrays concatenate (lowest priority first), anything else is replaced.
- Keys read so far: `model` (`provider/model`), `default_agent`, `shell`, `permissions` (list of `{action, resource, effect}`), `agents.<id>` (`model`, `system`, `description`, `steps`, `permissions`, `disabled`; unknown ids define new agents), `instructions` (extra files), `providers` (see `07`), `compaction{auto, buffer, keep}` `snapshots` (bool, see `03`) and `small_model` (see `06`). Other keys are ignored.
- Rule order per agent: built-in defaults, then top-level `permissions`, then the agent's own `permissions`; last match wins. This is how to make `shell` or `edit` ask, which the default ruleset does not:
```jsonc
{ "permissions": [
    { "action": "shell", "resource": "*",    "effect": "ask" },
    { "action": "shell", "resource": "ls *", "effect": "allow" }
] }
```
- Instructions: `<app dir>/config/AGENTS.md`, then `AGENTS.md` in each directory from the project root (nearest `.git`) down to the working directory, falling back to `CLAUDE.md` in a directory that has no `AGENTS.md` (a moco extension). Files are re-read from disk on every step instead of being sent as deltas, which keeps them live but costs prompt-cache hits when they change. `read` adds `AGENTS.md` files in directories between the file and the working directory, remembered per session (`instruction_state` table).
- Not done: remote config, managed/system config, live reload, `{file:}` path rules beyond "relative to the config file", URL instructions, `agents` markdown files, MCP (Phase 3, later slice).

## Top-level keys (`Config.Info`)

`shell`, `model` (`provider/model`), `default_agent`, `update` (disable|notify|auto), `share`, `username`, `timezone`, `permissions` (rule list),
`policies` (allow/deny providers, tools, integrations), `agents{id: AgentConfig}`, `snapshots` (bool), `watcher{ignore[]}`,
`formatter` (bool or per-formatter `{command[], extensions[], environment, disabled}`), `lsp` (same shape plus `initialization`),
`media{image{auto_resize,max_width,max_height}}`, `tool_output{max_lines,max_bytes}` (defaults 2000 / 51200), `mcp{timeout{startup,catalog,execution}, servers{name: Local|Remote}}`,
`compaction{tokens?,auto,keep,buffer,precompute}`, `skills[]` (extra paths/URLs), `commands{name:{template,description?,agent?,model?,subagent?}}`,
`instructions[]` (extra files/URLs), `references{name: {repository,branch?} | {path}}`, `websearch`, `plugins`, `worktree{directory}`, `warming`, `providers{id: ProviderConfig}`, `experimental{...}`.

AgentConfig: `model, request{settings,headers,body}, system, description, mode, hidden, color, steps, disabled, permissions`.
ProviderConfig: `name, env[], package, compatibility, models{id: {name, family, limit{context,input,output}, cost, capabilities, variants[], disabled, ...}}, settings, headers, body`.

## Instructions (AGENTS.md)

- Global: `<config dir>/AGENTS.md`. Project: `AGENTS.md` in every directory from the working dir up to the project root (or `$HOME` if the cwd is under it).
- Loaded at session start and watched for changes; changes are sent as instruction deltas.
- Also loaded lazily: when `read` touches a file, any `AGENTS.md` in directories between that file and the Location root (exclusive) is added for the session.
- Config `instructions[]` adds paths/URLs (remote ones fetched over HTTP, cached).
- `CLAUDE.md` compatibility: upstream reads `.claude` dirs for config/skills; I did not confirm `CLAUDE.md` as an instruction file name. Check `instruction-discovery.ts`.

## Skills

- A skill is a directory with `SKILL.md` (YAML frontmatter: at least `name`, `description`; body is the instructions) plus optional files. Discovered under `skills/` in each config root (`config/plugin/skill.ts`, `config/plugin/compatibility.ts` for `.claude`/`.agents`), configured paths, or remote index URLs (`SkillDiscovery`: an index JSON `{skills: [{name, version?, files[]}]}` with files downloaded to a cache; path segments are validated to prevent traversal).
- The model sees a list in the system context:
```
Skills provide specialized instructions and workflows for specific tasks.
Use the skill tool to load a skill when a task matches its description.
...
<available_skills>
  <skill><id>..</id><name>..</name><description>..</description></skill>
</available_skills>
```
- `skill` tool (or user `/skill` / `@skill` mention) injects `<skill_content>` (format in `04-tools.md`). Permission action `skill`.
- Frontmatter parsing is lenient: if YAML fails, retry treating unquoted values containing `:` as block scalars (`config/markdown.ts sanitize`).

## Custom commands

Defined in config `commands` or as markdown files in `command/` or `commands/` dirs of the config roots (frontmatter `description`, `agent`, `model`, `subagent`; body = template).
Template placeholder `$ARGUMENTS` (appended after a blank line if absent). Optional `subagent: true` runs it in a child session. Appears as `/name` in the prompt and command palette.

## MCP

Config per server:
- Local: `{type:"local", command:[...], cwd?, environment?, disabled?, codemode?, timeout?, protocol?}` -> spawn child process, speak MCP over stdio.
- Remote: `{type:"remote", url, headers?, oauth?: {client_id?, client_secret?, scope?, callback_port?, redirect_uri?, auth_server_metadata_url?} | false, disabled?, timeout?, protocol?}` -> MCP Streamable HTTP transport (the code uses the SDK's `StreamableHTTPClientTransport`; a legacy-SSE fallback was not seen); OAuth 2 with dynamic client registration and a local callback listener for the browser redirect.
- Timeouts (ms): `startup`, `catalog` (tools/list, prompts/list), `execution` (tool/prompt call).
- Status per server: `connected | pending | disabled | failed(error) | needs_auth(error)`. Events `mcp.status.changed`, `mcp.tools.changed`.
- Protocol: `legacy` (initialize handshake, revisions up to 2025-11-25) default; newer revision negotiable. Implement the 2025-xx spec first.
- Surface: tools (`<server>_<tool>`), prompts (as slash commands), resources (listed and readable through the `mcp-resource` tool and `@` mentions).
- Tool-list changes reload only the MCP source, preserving the order of other tool registrations.
- "Code Mode" (tools exposed through a generated code API instead of the native tool list) is an upstream optimization; **do not implement**: use the native tool list.

## References
`references{name: {repository, branch?, description?, hidden?} | {path, ...}}`: named external repos/dirs the agent can read; Git ones are cloned into `data/repos`. Optional.

## Formatters and LSP
After write/edit/patch upstream runs a configured formatter for the file extension and syncs BOM. LSP gives diagnostics. Both optional; formatters are a cheap win (spawn command with `$FILE`), LSP is a large effort.

## Project/worktree
Project = git root detection. `worktree` creates `git worktree`s for isolated sessions (`core/src/worktree/*`). Optional.

## moco implementation notes (Phase 3, skills / commands / mentions slice)

Code: `markdown.rs`, `skills.rs`, `commands.rs`, `mentions.rs`, `tools/skill.rs`.
- **Frontmatter** (`markdown.rs`): flat `key: value` pairs only. A value is everything after the first colon (so `description: Use when: ...` is fine), quotes are removed, `>` and `|` start an indented block, other indented lines continue the previous value, lines that are not `key: value` are ignored. No nesting, lists or anchors; a malformed header never fails a skill or command load.
- **Roots**: `~/.claude`, `~/.agents`, `<app dir>/config`, then for every directory from `/` to the working directory its `.claude`, `.agents` and `.moco`, lowest priority first (`.claude` and `.agents` are read so skills and command files written for other tools work). Skills live in `<root>/skills/<id>/SKILL.md`, commands in `<root>/commands/*.md` or `<root>/command/*.md`. Config adds `skills` (directories of skills, or one skill directory) and `commands`.
- **Skills**: the directory name is the id; `description` is required (a skill without one is skipped), `name` defaults to the id. A later root overrides an earlier skill with the same id. The sorted list is rendered into the system prompt as `<available_skills>` (stable order, so the prompt cache survives) and the `skill` tool loads one: `<skill_content>` with the body, the base directory, a note that relative paths resolve there, and up to 10 sibling files. The tool and the prompt section exist only when skills exist, and only for agents whose rules allow `skill` (the read-only `explore` agent gets neither). Skills are discovered once at startup. Remote skill indexes are not implemented.
- **Commands**: `/name args` expands the template (`$ARGUMENTS` replaced, otherwise the arguments are appended after a blank line) and sends it as a normal prompt. Frontmatter or config `agent` and `model` apply to that prompt only (restored afterwards); `subagent: true` runs it in a child session, prints the result and records it in the conversation as a synthetic message. Built-in `/init` and `/review` use templates written for moco; file and config commands override them by name. The chat's own commands (`/model`, `/undo`, ...) cannot be overridden. `moco -p "/name args"` works for non-subagent commands.
- **Mentions**: `@token` at the start of a word. A path (relative, absolute or `~/`) that exists attaches the file (up to 2000 lines / 50 KiB, files over 2 MiB and binary files are skipped with a note) or a directory listing (200 entries); otherwise a skill id loads that skill; otherwise a subagent name adds a reminder to delegate to it with the `subagent` tool. A file beats a skill or agent of the same name. Everything is read when the prompt is sent and stored with the message (`mentions`), so history replays as sent even if the files change. Images and PDFs mentioned with `@` are attached as media (see `04`); a path with backslash-escaped spaces, as a terminal pastes a dragged-in file, is understood. Not implemented: MCP resources, fuzzy completion.

## moco implementation notes (Phase 3, MCP over stdio)

Code: `mcp/client.rs` (JSON-RPC over a child process), `mcp/manager.rs` (lifecycle, catalogs, adapters), `mcp/trust.rs`.
- **Config**: `mcp.servers.<name>` with `command` (program and arguments), `cwd`, `environment` (added to the inherited one), `disabled`, `timeout` (per-call, ms); `mcp.timeout{startup, catalog, execution}` in ms (defaults 30 s, 30 s, 5 min). Entries with `type: "remote"` or a `url` are parsed but report `failed: remote MCP servers are not supported yet` (next slice).
- **Protocol**: newline-delimited JSON-RPC 2.0. moco offers revision `2025-06-18` and accepts the server's answer if it is one of `2025-11-25`, `2025-06-18`, `2025-03-26`, `2024-11-05`; anything else fails the handshake with a message naming both sides. Lists are paged. Server requests: `ping` is answered, everything else gets "method not supported" (moco declares no client capabilities, so no sampling, roots or elicitation). Non-JSON lines on stdout are ignored; stderr is kept (last 20 lines) and quoted in failure messages. `notifications/{tools,prompts,resources}/list_changed` re-read that server's lists, and the next request already uses them. A cancelled or timed-out call sends `notifications/cancelled`. Progress and logging notifications are ignored.
- **Status** per server: `pending`, `connected`, `disabled`, `failed(reason)` (also what a server that exited reports), `needs_auth` (reserved for remote servers). `/mcp` shows them, `/mcp reconnect <name>` restarts one. There is no automatic restart: the model gets "the server stopped ... the user can restart it with /mcp reconnect X" as the tool error.
- **Tools**: `<server>_<tool>` with characters outside letters, digits, `_` and `-` replaced by `_`, cut to 64 characters with a short hash when longer, `_2`, `_3` on a clash. The server's own description and input schema are passed through. The permission action is the full tool name with resource `*`, so `{"action": "fake_*", "resource": "*", "effect": "ask"}` works; "always" saves that. Results: text joined, images attached as media (subject to the model's capabilities and the usual limits), embedded resources as text, `isError` raised as a tool error the model can react to, `structuredContent` shown as JSON when there is no other content. An agent that denies everything it does not list (`explore`) does not see MCP tools, unless a rule in your own config matches them: your rules are evaluated after an agent's built-in ones, so they win.
- **Prompts** become slash commands `/<server>:<prompt>` (listed by `/commands`, also usable as `moco -p "/server:prompt args"`). Words after the command fill the prompt's arguments in order (the last one takes the rest), `name=value` sets one by name, quotes group words; a missing required argument prints the usage. The returned messages are sent as an ordinary prompt.
- **Resources** are readable through the `mcp-resource` tool (offered only while some server has resources; its description lists up to 50 of them) and `@server:name`, `@name` (when unique) or `@<uri>` in a prompt (text only, up to 2000 lines / 50 KiB, attached once however it is named). Resource templates and subscriptions are not implemented.
- **Trust**: a server entry is a command line, so a repository whose config declares one could run code on your machine just by being opened. Servers defined in your own configuration (`<app dir>/config/` or the file named by `$MOCO_CONFIG`) start as configured. Servers defined in a *project* config file ask first in a terminal (`y` this time, `a` always, anything else no); "always" is remembered in `data/mcp-trust.json` per project directory and exact definition (name, command, environment, `cwd`), so changing the command asks again. Without a terminal, such servers stay off with a warning unless `MOCO_TRUST_PROJECT_MCP=1`. The same reasoning will apply to formatter hooks.
- **Process handling**: each server runs in its own process group; on exit moco closes its stdin, then terminates the group. moco waits for servers to settle at startup (so the tool list is fixed for the conversation). Verified against a fake server written for the tests (`crates/moco-core/tests/fake_mcp.py`, needs `python3`; those tests skip without it) and with the live model on macOS 10.14; **not yet verified against a third-party server** such as the reference TypeScript servers (no Node on the test machine).
- **Not implemented**: remote servers (Streamable HTTP, OAuth), sampling, roots, elicitation, resource templates and subscriptions, completion, tasks, server-initiated logging display, per-tool enable/disable, "Code Mode".

