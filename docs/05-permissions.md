# Permissions

Sources: `core/src/permission.ts`, `schema/src/permission.ts`, `core/src/util/wildcard.ts`, `core/src/shell/parse.ts`, `core/src/file-access.ts`, `core/src/permission/saved.ts`.

## Model

A **rule** is `{action: string, resource: string, effect: "allow" | "deny" | "ask"}`. `action` and `resource` are wildcard patterns.
A **request** is `{action, resources: string[], save?: string[], metadata?, source, message?}`.

Actions in use: `read`, `edit` (covers write/edit/patch), `glob`, `grep`, `shell`, `webfetch`, `websearch`, `skill`, `subagent`, `question`,
`external_directory`, plus each MCP tool name.

## Wildcard match (`util/wildcard.ts`)

- Normalize `\` to `/`. Escape regex metacharacters; `*` -> `.*` (matches anything including `/`); `?` -> `.`.
- Whole-string, dotall match.
- Special: a pattern ending in ` *` (space star) also matches the bare command, so `ls *` matches `ls` and `ls -la`.

## Evaluation

```
evaluate(action, resource, rulesets...) = last rule in concatenated rules whose action AND resource patterns match; default {effect: "ask"}
```

Rule order for a request = `agent.permissions` then `session.permission` (session overrides), then user-saved "always" rules (all `allow`) appended.

Decision for a request with several resources:
1. If **any** resource evaluates to `deny` against the configured rules (agent + session) -> **deny** (saved rules cannot override a configured deny).
2. Else evaluate each resource against configured + saved rules. If any is `ask` -> **ask**, else **allow**.
3. Plugin hook may adjust the result. (Skip.)
4. "Unattended" mode (scheduled runs): an `ask` becomes `allow` only if an extra unattended ruleset allows it; otherwise **deny** with message `Permission <action> needs approval, and nobody is present to give it`.

Outcomes: `allow` -> proceed. `deny` -> tool fails with `BlockedError` (message `Permission denied: <action>`, lists relevant rules). `ask` -> create a pending request, publish `permission.asked`, suspend the tool until replied.

## Replies

`once` | `always` | `reject` (with optional feedback message).
- `once`: continue.
- `always`: continue and persist `(project, action, each save pattern)` as saved allow rules; then re-evaluate other pending requests and auto-approve those now allowed.
- `reject` without message: fail with `DeclinedError` (stops the run of this tool batch). With message: `CorrectedError{feedback}`, model sees it as the tool error and continues. Rejection applies to **all** pending requests of that session.
- On shutdown all pending requests are rejected.

**Removing saved rules (moco).** Upstream has no way to take an "always" back. In moco, `/permissions` lists the project's saved rules with numbers, `/permissions remove <n>` (or `remove <action> <pattern>`) forgets one and `/permissions clear` forgets all of them. `moco permissions [--dir DIR] [remove N | remove ACTION PATTERN | clear]` does the same without a session, for the current directory's project or `DIR`'s, and never creates a project. A removal takes effect at the next permission check of a running step in the same process (the step's copy of the saved rules is dropped, `store::SAVED_PERMISSIONS_GEN`); one made by another moco process takes effect from that session's next step. Saved rules are appended after the configured ones, so a configured `ask` cannot narrow a saved `allow`: remove the broad saved rule, then answer "always" to the narrower request or add an `allow` rule for exactly what you want.

## Permission modes (moco)

moco has two modes. The default is `manual`. The mode sets the starting rules of every agent (built-in and config-defined); your own `permissions` rules come after it and win.

**Choosing a mode.** Each session has its own mode, shown in the status line as `manual` or `auto`:
- Full-screen chat: Shift-Tab switches it. `/mode [manual|auto]` does the same, and `/mode` alone shows it.
- Plain chat: `/mode [manual|auto]`.
- `--permission-mode manual|auto` sets it for this session and is the default for new ones. `permission_mode` in config sets the default when no flag is given.

A switch applies from the next step of a run: a tool call already running is not interrupted, and the next step's prompts follow the new mode. A subagent takes the mode its parent had when it started.

- **manual** (default): `*` asks. Allowed without a prompt: `read`, `glob`, `grep`, `question`, `todowrite`, `skill`. Reading `.env` files asks, `.env.example` is allowed, `websearch` asks, and so does `external_directory` (it falls under `*`).
- **auto**: the upstream default ruleset below. Everything is allowed except `.env` reads and `websearch`.

The `explore` subagent denies `shell` in both modes, because a shell command can write and the rules must enforce its read-only role.

Not done: the automatic check that is meant to replace the prompt in auto mode.

## Default ruleset (`auto`, and the upstream base)

```
{ "*", "*", allow }
{ read, "*.env",         ask }
{ read, "*.env.*",       ask }
{ read, "*.env.example", allow }
```
(`schema/src/agent.ts`). Built-in agents append more rules (see `06-agents-and-prompts.md`). Users append through config `permissions` and per-agent `permissions`.

**Project config can only tighten.** Rules are last-match-wins, so a cloned repository's `moco.json` must not be able to append an `allow` (or an `ask` after the user's `deny`) and relax your rules or a built-in agent's restrictions (e.g. explore's edit deny). In config files other than the app config dir and `$MOCO_CONFIG`, only `deny` rules are kept, both in top-level `permissions` and `agents.<id>.permissions`; the rest are dropped with a warning. Put allow/ask rules in your own config.

## Resource strings per tool

| Tool | resources | save (for "always") |
|---|---|---|
| read | path relative to Location (absolute if external) | `*` |
| edit/write | relative path | `*` |
| patch | per file (see `patch.ts`) | |
| glob/grep | the pattern | `*` |
| webfetch | the URL | `*` |
| shell | one per parsed command, e.g. `git push origin main` | arity-based prefix + ` *`, e.g. `git push *` |
| external_directory | `<dir>/*` | `<project root of dir>/*` |

Tools that show a file's content also check `read` on it, as the read tool does (moco, #119): `grep` on a file path, `edit` of an existing file, and a patch `Update File` (with or without `Move to`) of an existing file. Ordinary files pass silently because `read *` is allowed in both modes; `.env` asks even where edits are allowed. A `grep` directory walk does not prompt: files the built-in `.env` rules protect are left out and listed in a note, and grepping or reading one by path asks. Config rules apply to the paths a tool names; a walk cannot see them without prompting.

### Shell command scanning (`shell/parse.ts`)

1. Tokenize the command string with a real shell grammar (upstream: tree-sitter bash/powershell). Extract every simple command, including those inside `&&`, `||`, `;`, pipes, `$(...)` substitutions and subshells.
2. For each command build `resource` = the normalized command text and `save` = command prefix up to its **arity** plus ` *`. Arity table maps head words to how many leading words identify the command: e.g. `cat/ls/rm/echo...` = 1, `git` = 2 (`git push`), `npm run` = 3 (`npm run dev`), `bun run` = 3, `cargo add` = 3, `aws/az` = 3. The table is in `parse.ts` `ARITY` (copy it).
3. Collect directory arguments from `cd/pushd/popd/chdir` and from path-valued flags; resolve each and run external-directory checks before the command permission.
4. Skip declarations (`FOO=bar` alone). Redirection targets are folded into the resource.
5. If parsing fails: treat the whole command as one resource.
Simplification for v1: split on `;`, `&&`, `||`, `|`, newline with a quote-aware splitter, and treat `$()`/backticks as unparsed (ask).

Heredocs (moco, `tools/shellparse.rs`): `<<WORD`, `<<'WORD'`, `<<"WORD"` and `<<-WORD` are input to the command they belong to, not commands. The body, up to the delimiter line, is appended to that command's resource text, so `osascript *` covers `osascript <<'EOF' ...`, a deny rule can still match text in the body, and "always" saves no patterns for body lines. The classifier sees the whole command, and `bash <<EOF` still never reaches it (shells are on the never-classify list). These cases are opaque (always ask) because the shell might read them differently: a body with no delimiter line, a delimiter that is missing or uses `$`, and in an unquoted body `$(...)`, backticks, or a line ending in `\` (bash and sh join it with the next line before comparing it with the delimiter, so `EO\` + `F` ends the body). `<<<` is a here-string and has no body.

## UI contract

Pending request shows action, resources, tool preview (for edit: the diff; for shell: command and cwd), and buttons Allow once / Allow always / Reject (with optional feedback text). Command palette lists pending permissions across sessions.
