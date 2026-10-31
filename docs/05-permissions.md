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

## Default ruleset for a new agent

```
{ "*", "*", allow }
{ read, "*.env",         ask }
{ read, "*.env.*",       ask }
{ read, "*.env.example", allow }
```
(`schema/src/agent.ts`). Built-in agents append more rules (see `06-agents-and-prompts.md`). Users append through config `permissions` and per-agent `permissions`.

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

### Shell command scanning (`shell/parse.ts`)

1. Tokenize the command string with a real shell grammar (upstream: tree-sitter bash/powershell). Extract every simple command, including those inside `&&`, `||`, `;`, pipes, `$(...)` substitutions and subshells.
2. For each command build `resource` = the normalized command text and `save` = command prefix up to its **arity** plus ` *`. Arity table maps head words to how many leading words identify the command: e.g. `cat/ls/rm/echo...` = 1, `git` = 2 (`git push`), `npm run` = 3 (`npm run dev`), `bun run` = 3, `cargo add` = 3, `aws/az` = 3. The table is in `parse.ts` `ARITY` (copy it).
3. Collect directory arguments from `cd/pushd/popd/chdir` and from path-valued flags; resolve each and run external-directory checks before the command permission.
4. Skip declarations (`FOO=bar` alone). Redirection targets are folded into the resource.
5. If parsing fails: treat the whole command as one resource.
Simplification for v1: split on `;`, `&&`, `||`, `|`, newline with a quote-aware splitter, and treat `$()`/backticks as unparsed (ask).

## UI contract

Pending request shows action, resources, tool preview (for edit: the diff; for shell: command and cwd), and buttons Allow once / Allow always / Reject (with optional feedback text). Command palette lists pending permissions across sessions.
