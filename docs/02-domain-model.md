# Domain model and storage

Sources: `packages/schema/src/session*.ts`, `core/src/session/{sql,inbox,store,message}.ts`, `core/src/session/AGENTS`-style notes in repo `AGENTS.md`.

## Entities

- **Project**: a directory (usually a git root). ID derived from the repo. Has icon, name.
- **Location**: the working directory of a session inside a project (plus optional subpath). Services such as tools and permissions are scoped to it.
- **Session**: a conversation. Fields (table `session_v2`): `id`, `project_id`, `parent_id` (subagent child), `fork_session_id` + `fork_boundary`,
  `slug`, `directory`, `title`, `version`, `share_url`, summary counts (`additions/deletions/files`), `metadata` (json), `cost`,
  `tokens_{input,output,reasoning,cache_read,cache_write}`, `revert` (json), `permission` (json ruleset), `agent`, `model {id, providerID, variant?}`,
  timestamps, `time_busy`, `time_idle`, `time_viewed`, `idle_outcome`, `time_compacting`, `time_archived`, an execution claim timestamp and `resume_attempts`.
- **Inbox item** (table `session_inbox`): durable pending work: user prompt, synthetic message, compaction request, or move. Unconsumed only.
- **Message**: tagged union by `type`:
  `user` (text, files, agents, skills), `synthetic` (text, description), `system` (model-facing update text frozen at emit time + short description),
  `skill` (skill id/name/text), `shell` (user-run command: shellID, command, status, exit, output),
  `agent-switched`, `model-switched`, `location-switched`, `assistant` (ordered content parts), `compaction`.
  All have `id` (`msg_...`), `metadata`, `time.created`.
- **Assistant content parts**: `text`, `reasoning`, `tool` (state machine below), plus provider-owned `native` replay data echoed back verbatim.
- **Tool state**: `streaming {input: string}` -> `running {input, metadata}` -> `completed {input, output, content, metadata, time}` or `error {...}`.
- **Permission request**: `{id per_..., sessionID, action, resources[], save[]?, metadata?, source {type: "tool", messageID, id}, message?}`.
- **Saved permission**: `(project_id, action, resource)` rows that act as `allow` rules.
- **Todo list**: per session list of `{content, status: pending|in_progress|completed|cancelled}`.
- **File diff** (`schema/src/file-diff.ts`): per-file before/after patches used for revert and review.

## Events

State changes are published as events and folded into tables by projectors. A simpler rewrite can write tables directly and publish
the same event names to the UI. Names used upstream (UI subscribes to these):

```
session.{created,updated,deleted,renamed,archived,unarchived,forked,moved,viewed,status,idle,error,diff,permissions,metadata.updated,
         agent.selected,model.selected,synthetic,skill.activated,instructions.updated}
session.inbox.{enqueued,delivered,cancelled,delivery.changed}
session.execution.{started,succeeded,failed,interrupted}
session.step.{started,streamed,ended,failed}
session.text.{started,delta,ended}   session.reasoning.{started,delta,ended}
session.tool.{input.started,input.delta,input.ended,called,progress,success,failed}
session.compaction.{started,delta,ended,failed}
session.shell.{started,ended}   session.retry.scheduled   session.usage.{recorded,updated}
session.revert.{staged,cleared,committed}   session.message.content.updated
permission.{asked,replied}  form.{created,replied,cancelled}  mcp.{status.changed,tools.changed,resources.changed}
pty.{created,updated,exited,deleted}  shell.{created,exited,deleted}  vcs.branch.updated  file.edited  config.updated  project.updated
```

Durable-event rule (repo `AGENTS.md`): record only irreducible new facts; derive the rest by folding history. Keep ephemeral events
(deltas, permission asks) out of the durable log.

## Prompt admission and delivery (important semantics)

1. `prompt(session, input, {resume?})` inserts an inbox row (durable), then wakes the session runner unless `resume:false`.
2. The runner **delivers** inbox items at safe boundaries: delivery consumes the inbox row and inserts the visible message in one transaction.
3. Delivery modes: **steer** (default; delivered at the next step boundary, mid-run) and **queue** (delivered one at a time when idle).
   At a step boundary, steered compaction goes first, then other steers in enqueue order. At idle, steers go first, else exactly one queued item.
   Items can be cancelled or switched between queue and steer before delivery.
4. Re-sending the same inbox item ID is idempotent (first admission wins).
5. A new user input resets the agent's step allowance; a batch of steers resets it once.
6. One session runs at a time; different sessions run concurrently. A write-ahead "execution claim" marks busy; on crash, startup resumes
   claimed top-level sessions with a bounded attempt counter.

## Storage

SQLite, WAL mode. Minimum tables for the rewrite: `project`, `session`, `message` (or parts split out: upstream stores assistant content in a parts table),
`session_inbox`, `permission_saved`, `credential`, `kv`, `blob` (large attachments by hash), `instruction_state`, `todo`, `snapshot_ref`.
JSON columns for variable data. Use a migrations table; upstream uses timestamped migrations in `core/src/database/migration`.

Filesystem layout (upstream uses XDG; on macOS pick `~/Library/Application Support/<app>/`):
- `data/` DB, logs, cloned `repos/`, tool-output spill files
- `config/` global `opencode.json(c)`, global `AGENTS.md`
- `cache/` downloaded model catalog, installed binaries

Credentials: upstream keeps API keys in its DB. moco keeps them in a separate `auth.json` in the app data directory, created with mode `0600`, not in SQLite. No Keychain: the app must work headless (SSH, no GUI session), and the Phase 0 spike showed the login keychain is unusable there (`-25308 User interaction is not allowed`).
