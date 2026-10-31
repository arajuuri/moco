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
- Keys read so far: `model` (`provider/model`), `default_agent`, `shell`, `permissions` (list of `{action, resource, effect}`), `agents.<id>` (`model`, `system`, `description`, `steps`, `permissions`, `disabled`; unknown ids define new agents), `instructions` (extra files), `providers` (see `07`), `compaction{auto, buffer, keep}` and `snapshots` (bool, see `03`). Other keys are ignored.
- Rule order per agent: built-in defaults, then top-level `permissions`, then the agent's own `permissions`; last match wins. This is how to make `shell` or `edit` ask, which the default ruleset does not:
```jsonc
{ "permissions": [
    { "action": "shell", "resource": "*",    "effect": "ask" },
    { "action": "shell", "resource": "ls *", "effect": "allow" }
] }
```
- Instructions: `<app dir>/config/AGENTS.md`, then `AGENTS.md` in each directory from the project root (nearest `.git`) down to the working directory, falling back to `CLAUDE.md` in a directory that has no `AGENTS.md` (a moco extension). Files are re-read from disk on every step instead of being sent as deltas, which keeps them live but costs prompt-cache hits when they change. `read` adds `AGENTS.md` files in directories between the file and the working directory, remembered per session (`instruction_state` table).
- Not done: remote config, managed/system config, live reload, `{file:}` path rules beyond "relative to the config file", URL instructions, `agents` markdown files, skills, custom commands, MCP.

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
