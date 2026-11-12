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
- Project config files (anything but `<app dir>/config/` and `$MOCO_CONFIG`) cannot direct your secrets anywhere (#116): `{env:}` and `{file:}` are not expanded there (left as written, with a warning); `websearch` keeps only `false` and `max_results`; `providers.<id>.request` and `models.<id>.request`/`variants` are ignored like `base_url`/`env`/`protocol`/`headers` (`07`); `instructions` entries must be relative paths inside the project (no `~`, absolute or `..`). Each dropped key gets a warning.
- They also cannot decide what is the user's (#168). `small_model` is read only from your own config, because the permission classifier falls back to it, and a project must not choose the model that judges its own commands. `limits` (`maxCostPerRun`, `maxCostPerSession`, `maxSteps`) may only be lowered: a value above the one in force (yours, or an outer project's) is ignored with a warning, while setting a limit you left open is allowed. A project that replaces a built-in agent's `system` prompt (`build`, `plan`, `general`, `explore`), or the built-in `/review` or `/init` command (through `commands` in its config or a command file in its `.moco/`, `.claude/` or `.agents/` directory), still does so, but each start names the file in a warning.
- Keys read so far: `permission_mode`, `model` (`provider/model`), `default_agent`, `shell`, `permissions` (list of `{action, resource, effect}`), `agents.<id>` (`model`, `system`, `description`, `steps`, `permissions`, `disabled`; unknown ids define new agents), `instructions` (extra files), `providers` (see `07`), `compaction{auto, buffer, keep}` `snapshots` (bool, see `03`) and `small_model` (see `06`). Other keys are ignored.
- `permission_mode`: `"manual"` (default) or `"auto"`, see `05`. It is the mode of sessions that have not chosen one; `moco --permission-mode` sets the mode of the session it starts, and Tab or `/mode` change it. Any other value is an error. A project's config may only set `"manual"`; like `permission_preset` and loosening `sandbox` settings, anything else there is ignored with a warning (`05`).
- Rule order per agent: mode defaults, then top-level `permissions`, then the agent's own `permissions`; last match wins. In auto mode, this is how to make `shell` or `edit` ask (manual mode already asks for them):
```jsonc
{ "permissions": [
    { "action": "shell", "resource": "*",    "effect": "ask" },
    { "action": "shell", "resource": "ls *", "effect": "allow" }
] }
```
- Instructions: `<app dir>/config/AGENTS.md`, then `AGENTS.md` in each directory from the project root (nearest `.git`, see `04-tools.md`) down to the working directory, falling back to `CLAUDE.md` in a directory that has no `AGENTS.md` (a moco extension). Files are re-read from disk on every step instead of being sent as deltas, which keeps them live but costs prompt-cache hits when they change. `read` adds `AGENTS.md` files in directories between the file and the working directory, remembered per session (`instruction_state` table).
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
- **Mentions**: `@token` at the start of a word. A path (relative, absolute or `~/`) that exists attaches the file (up to 2000 lines / 50 KiB, files over 2 MiB and binary files are skipped with a note) or a directory listing (200 entries); otherwise a skill id loads that skill; otherwise a subagent name adds a reminder to delegate to it with the `subagent` tool. A file beats a skill or agent of the same name. Everything is read when the prompt is sent and stored with the message (`mentions`), so history replays as sent even if the files change. Images and PDFs mentioned with `@` are attached as media (see `04`); a path with backslash-escaped spaces, as a terminal pastes a dragged-in file, is understood. Not implemented: fuzzy completion.
- **Whose mentions** (#124): a mention the user typed attaches what it names (typing `@.env` answers the `.env` ask); only a `deny` rule for `read` (or `mcp-resource`) skips it. The same holds for built-in commands and the user's own commands (files directly in `~/.claude`, `~/.agents` or `<app dir>/config` `commands/`, and `commands` in their own config). In a project's command (`.moco`/`.claude`/`.agents` of the project or its parents, a project config's `commands`) and in an MCP prompt's text, a path is attached only when it is inside the working directory or project root after symlinks are resolved and the session's rules allow `read` of it without asking (so `.env` is not, in either mode), and an MCP resource only when `mcp-resource` is allowed without asking or, for an MCP prompt, it is that server's own resource. Anything else stays plain text with a note (`left @x as text: ...`); the model can still read it with the read tool, which asks as usual. Nothing asks at submit time, so the TUI and `-p` behave alike. A word the user typed after the command name counts as theirs (`Review @$ARGUMENTS` with `../notes.md` attaches it); the arguments of an MCP prompt do not, since the server decides what it returns.

## moco implementation notes (Phase 3, MCP over stdio)

Code: `mcp/client.rs` (JSON-RPC over a child process), `mcp/manager.rs` (lifecycle, catalogs, adapters), `mcp/trust.rs`.
- **Config**: `mcp.servers.<name>` with `command` (program and arguments), `cwd`, `environment` (added to the inherited one), `disabled`, `timeout` (per-call, ms); `mcp.timeout{startup, catalog, execution}` in ms (defaults 30 s, 30 s, 5 min). Entries with `type: "remote"` or a `url` are parsed but report `failed: remote MCP servers are not supported yet` (next slice).
- **Protocol**: newline-delimited JSON-RPC 2.0. moco offers revision `2025-06-18` and accepts the server's answer if it is one of `2025-11-25`, `2025-06-18`, `2025-03-26`, `2024-11-05`; anything else fails the handshake with a message naming both sides. Lists are paged. Server requests: `ping` is answered, everything else gets "method not supported" (moco declares no client capabilities, so no sampling, roots or elicitation). Non-JSON lines on stdout are ignored; stderr is kept (last 20 lines) and quoted in failure messages. `notifications/{tools,prompts,resources}/list_changed` re-read that server's lists, and the next request already uses them. A cancelled or timed-out call sends `notifications/cancelled`. Progress and logging notifications are ignored.
- **Status** per server: `pending`, `connected`, `disabled`, `failed(reason)` (also what a server that exited reports), `needs_auth` (reserved for remote servers). `/mcp` shows them, `/mcp reconnect <name>` restarts one. There is no automatic restart: the model gets "the server stopped ... the user can restart it with /mcp reconnect X" as the tool error.
- **Tools**: `<server>_<tool>` with characters outside letters, digits, `_` and `-` replaced by `_`, cut to 64 characters with a short hash when longer, `_2`, `_3` on a clash. The server's own description and input schema are passed through. The permission action is the full tool name with resource `*`, so `{"action": "fake_*", "resource": "*", "effect": "ask"}` works; "always" saves that. Results: text joined, images attached as media (subject to the model's capabilities and the usual limits), embedded resources as text, `isError` raised as a tool error the model can react to, `structuredContent` shown as JSON when there is no other content. An agent that denies everything it does not list (`explore`) does not see MCP tools, unless a rule in your own config matches them: your rules are evaluated after an agent's built-in ones, so they win.
- **Prompts** become slash commands `/<server>:<prompt>` (listed by `/commands`, also usable as `moco -p "/server:prompt args"`). Words after the command fill the prompt's arguments in order (the last one takes the rest), `name=value` sets one by name, quotes group words; a missing required argument prints the usage. The returned messages are sent as an ordinary prompt.
- **Resources** are readable through the `mcp-resource` tool (offered only while some server has resources; its description lists up to 50 of them) and `@server:name`, `@name` (when unique) or `@<uri>` in a prompt (text only, up to 2000 lines / 50 KiB, attached once however it is named). Resource templates and subscriptions are not implemented.
- **Trust**: a server entry is a command line, so a repository whose config declares one could run code on your machine just by being opened. Servers defined in your own configuration (`<app dir>/config/` or the file named by `$MOCO_CONFIG`) start as configured. Servers defined in a *project* config file ask first in a terminal (`y` this time, `a` always, anything else no); "always" is remembered in `data/mcp-trust.json` per project directory and exact definition. The prompt shows, and the fingerprint covers, everything that changes what runs or where data goes: the command and arguments, `cwd`, `environment`, `timeout`, and for a remote server the URL, `headers` and `oauth` settings, so changing any of them asks again. Environment and header values are shown as written in the project file, so a `{env:NAME}`/`{file:path}` value appears as that template (marked "substituted when used"), never as the secret. A value the project entry does not set itself (configs merge per key, so a project that only changes the `url` of a server you defined keeps your `headers`) is shown by name and length as coming from another config file, with a note that it would be sent to the URL shown. The fingerprint is a SHA-256 over the substituted definition, so a changed secret also asks again. Values are cut at 120 characters and control characters are escaped. The approval covers the definition, not the files it runs: when the command names a file inside the project (`python ./mcp/server.py`, `./scripts/fmt.sh`), the prompt says that approving also covers later versions of it. Approvals given before these fields were shown (fingerprints without a version) no longer match and ask once more; a damaged trust file reads as empty. Without a terminal, such servers stay off with a warning unless `MOCO_TRUST_PROJECT_CONFIG=1` (the older name `MOCO_TRUST_PROJECT_MCP=1` also works). The same gate applies to formatters, checks and a project's `shell` executable.
- **Process handling**: each server runs in its own process group; on exit moco closes its stdin, then terminates the group. moco waits for servers to settle at startup (so the tool list is fixed for the conversation). Verified against a fake server written for the tests (`crates/moco-core/tests/fake_mcp.py`, needs `python3`; those tests skip without it) and with the live model on macOS 10.14; **not yet verified against a third-party server** such as the reference TypeScript servers (no Node on the test machine).
- **Not implemented**: remote servers (Streamable HTTP, OAuth), sampling, roots, elicitation, resource templates and subscriptions, completion, tasks, server-initiated logging display, per-tool enable/disable, "Code Mode".

## moco implementation notes (Phase 3, formatter hooks)

Code: `format.rs`, `ToolCtx::format_file`, the `write`, `edit` and `patch` tools, and `mcp/trust.rs` for approval.
- **Config**: `formatter` is `false` or a map of named formatters, each `{command, extensions, environment, disabled}`. `command` is a program and arguments in which `$FILE` is replaced by the file's path (the path is appended as the last argument when no argument contains it); `extensions` are matched case-insensitively against the end of the file name, with or without the leading dot. Maps merge by name across config files; a boolean replaces everything defined before it. There are no built-in formatters and no autodetection: only what the config names runs. `true` is accepted and does nothing. Example:
```jsonc
{ "formatter": {
    "rustfmt":  { "command": ["rustfmt", "--edition", "2021", "$FILE"], "extensions": [".rs"] },
    "prettier": { "command": ["npx", "prettier", "--write", "$FILE"], "extensions": [".js", ".ts", ".json", ".css"] } } }
```
- **When**: after `write`, `edit` and `patch` have written a file (patch: every added or updated file, not deleted ones). Only files inside the working directory or project are formatted. The first formatter in name order that covers the extension runs, in the working directory, with the entry's `environment` added, no stdin, a 30 second limit (the process group is killed on timeout); its output is only used to explain a failure.
- **What the model is told**: nothing when the file was not covered or was already formatted. If the formatter changed the file: "Formatted with X: the file now differs from what you wrote in these lines (every other line is as you wrote it, so there is no need to read the file again):" followed by the changed lines as `-N: old` / `+N: new` (the first 30; numbers are the file's lines after formatting), so the next `edit` is built on what is really there. When the lines cannot be shown (the file is not text) it says "...so read it again before editing it further." instead. If it failed: "The formatter X failed (exit code N: first lines of its output); the file was left as written." A formatter problem never fails the tool call. A byte-order mark the file had before is put back if the formatter dropped it.
- **Trust**: a formatter is a command, so one defined in a *project* config file needs approval exactly like an MCP server (`trust::fingerprint_formatter`; asked in a terminal, remembered per project and definition, off without a terminal unless `MOCO_TRUST_PROJECT_CONFIG=1`). Formatters in your own config (`<app dir>/config/` or `$MOCO_CONFIG`) just work. The exact command, its extensions and environment are shown before you answer.
- **Not implemented**: LSP diagnostics, built-in formatter presets, formatting files outside the project, a per-file-pattern exclude list, running a formatter on files changed by `shell` commands.


## moco implementation notes (Phase 3, slice G: MCP over Streamable HTTP with OAuth)

Code: `mcp/http.rs` (transport), `mcp/oauth.rs` (sign-in and token store), `mcp/client.rs` (`McpConn` trait shared with stdio), `mcp/manager.rs` (`authorize`, `logout`, `Status::NeedsAuth`).
- **Selection**: a server with a `url` is remote, one with a `command` is local; a remote entry without `url` fails with a clear status. `headers` (with `{env:}` substitution) are sent on every request.
- **Transport**: every JSON-RPC message is a POST with `Accept: application/json, text/event-stream`; the reply is JSON or an SSE stream that is read until the matching response. `Mcp-Session-Id` is kept and echoed, `MCP-Protocol-Version` is sent after initialize, and shutdown sends DELETE. A 404 on a session re-initializes once and retries. A GET stream is opened for server-initiated messages when the server allows it (405 is fine). Cancellation and timeouts behave as for stdio. Up to 3 redirects are followed, and only within the same origin (scheme, host and port), by sending the same request to the new address; a redirect to another host fails with an error naming it, so the configured headers, the token and the session id never leave the configured server. A JSON answer, an SSE line and an SSE event are each capped at 64 MiB; a larger one fails the request with a clear error. The legacy HTTP+SSE transport is not supported.
- **OAuth**: a 401 triggers a silent token refresh once; if that is not possible the server is `needs authorization` and `/mcp auth <name>` runs the flow: protected-resource metadata (RFC 9728, from `WWW-Authenticate` or well-known) then authorization-server metadata (RFC 8414, or `auth_server_metadata_url`, or default endpoints), dynamic client registration (RFC 7591) unless `client_id` is configured, authorization code with PKCE S256 and the `resource` parameter, a loopback listener (`callback_port` or a random port) or a pasted redirect address, and a code-for-token exchange. Stored per server URL in `<app dir>/mcp-auth.json` (mode 600): client, token endpoint, access and refresh token, expiry. A registered client is reused only with the same redirect address.
- **Not verified**: tested against `tests/fake_mcp_http.py` (which logs client protocol violations) and live on Mojave with a real model, but never against a hosted MCP service or real authorization server. Not implemented: client-credentials flow, token revocation on logout (tokens are only deleted locally), resumable SSE streams (`Last-Event-ID`).

## moco implementation notes (websearch config)

`websearch`: `false` (tool off), or `{provider?, url?, max_results?, key_env?[]}`. `provider` is `duckduckgo` (default), `brave`, `tavily`, `exa` or `searxng`; `url` replaces the provider's endpoint (required for `searxng`); `key_env` lists extra environment variables to read the key from. Only `false` and `max_results` are read from a project's config; `provider`, `url` and `key_env` decide where the query and key go, so they must come from your own config. Searches that carry a key (`brave`, `tavily`, `exa`) follow no redirects (the key header would go to the redirect target); a redirect fails with the address to put in `url`. Upstream asks on first use which provider to use; moco uses the config instead and asks for permission on use. See `04`.

## moco implementation notes (reasoning effort config)

- Top-level `effort`: default level for every session (`low`, `medium`, `high`, `xhigh`, `max`, `none`, ...); `agents.<id>.effort` overrides it per agent; the session's own choice (`/effort`) overrides both. See `07`.
- `providers.<id>.request` and `providers.<id>.models.<model>.request`: `{ "headers": {...}, "body": {...} }` added to every request (body objects are merged deeply, `null` deletes a field; the provider's first, then the model's).
- `providers.<id>.models.<model>.variants.<name>`: a level you define, `{ "headers": {...}, "body": {...} }`, listed after the built-in ones in `/effort`.
