# Agent loop

Sources: `core/src/session/runner/{llm,step,retry,max-steps,to-llm-message}.ts`, `session/compaction.ts`, `core/src/model-retry.ts`,
`tool/plugin/subagent.ts`, `core/src/tool-output.ts`.

## Run loop (per session)

```
drain(session):
  claim execution
  loop:
    deliver inbox items allowed at this boundary         (see 02-domain-model)
    if nothing to run and nothing queued: release claim; publish session.idle; stop
    runStep(step++)
```

`runStep(n)` (one logical LLM call):

1. Load context: system prompt + instructions + history (messages since the last compaction, plus the compaction summary).
2. **Auto compaction check**: estimate prompt size (provider-reported tokens from the last response + estimate of text since).
   If it exceeds the model limit minus buffer, run compaction, then restart this step with a fresh assistant message ID.
3. If `agent.steps` is set and `n >= steps`: append an assistant message with the MAX_STEPS text (below) and send with `toolChoice: none`
   (keep tool definitions in the request so the provider prompt cache prefix stays valid).
4. Send the streaming request. For each provider event: publish text/reasoning/tool-input deltas; on a complete `tool-call` event
   (not provider-executed) immediately start executing that tool concurrently. Read the stream to its end even after the finish event.
5. When the stream ends and all tool fibers finish, record usage/cost and a step snapshot, publish `step.ended`.
6. Outcome:
   - `Completed{needsContinuation}`: continue if the step produced tool calls (the model must see results), else go idle.
   - `Retry` / `Continue`: wait per backoff and re-run.
   - `RecoverFull`/`Compacted`: provider rejected context as too long before any output -> compact once, retry (only once per step: `recoverOverflow` flag).
   - `OutputLimit`: model hit max output tokens; may need continuation. (moco: `StepOutcome::CutOff`, continued with a stored
     synthetic message at most twice in a row; the output budget follows the model's `limit.output`, see archive#148.)

Each step captures a filesystem **snapshot** before local tools can run (taken in parallel with the request) so edits can be diffed and reverted.

### MAX_STEPS text (verbatim from `runner/max-steps.ts`)

```
CRITICAL - MAXIMUM STEPS REACHED

The maximum number of steps allowed for this task has been reached. Tools are disabled until next user input. Respond with text only.

STRICT REQUIREMENTS:
1. Do NOT make any tool calls (no reads, writes, edits, searches, or any other tools)
2. MUST provide a text response summarizing work done so far
3. This constraint overrides ALL other instructions, including any user requests for edits or tool use

Response must include:
- Statement that maximum steps for this agent have been reached
- Summary of what has been accomplished so far
- List of any remaining tasks that were not completed
- Recommendations for what should be done next

Any attempt to use tools is a critical violation. Respond with text ONLY.
```

## Retry policy (`model-retry.ts`)

- Retry only retryable provider/transport errors (rate limit, provider 5xx, timeouts, connection reset). Not auth, not invalid request.
- Backoff: exponential from 2 s, capped at 10 s per gap (2, 4, 8, then 10 s), at most **10 retries**, jittered.
- If the provider sends a retry-after (rate limit or internal error), wait at least that long, capped at **15 minutes**.
- Timeout errors are limited to **3** retries.
- Before each wait publish `session.retry.scheduled {attempt, at, error}` so the UI shows a countdown.
- One-shot generations (title, summary) retry only errors where the request provably never ran (`ECONNREFUSED`, `ENOTFOUND`, `EAI_AGAIN`), 3 times.

## Tool execution

- Tools in one step run concurrently; each starts as soon as its call is complete.
- Tool input is validated against its JSON schema; failures return a tool error to the model (so it can fix its call). Upstream also has a
  repair hook (`plugin/tool-input-repair.ts`) for malformed input.
- Output goes through `truncate`: if text is over **2000 lines or 50 KiB**, keep whole lines up to the limits, append
  `[showing lines 1-N of M; full output saved to <file>]`, and write the full output to a file under the data dir (metadata `truncated:true, outputPath`).
- Typed tool failures become model-visible tool errors. A user **decline without feedback** aborts the whole step's tool batch and the run stops;
  a decline **with feedback** becomes a tool error carrying the feedback text and the model continues. Rejecting one pending permission rejects all pending
  permissions of that session (same feedback applied).
- Interrupting the session cancels in-flight tools; their parts end as `Tool execution interrupted` / `Step interrupted`.
- Missing tool result from provider: `Provider did not return a tool result`.

## Compaction (`session/compaction.ts`)

Settings: `auto` (bool), `buffer` (percent or tokens; default keep 15% and at least 16k free), `keep` (tokens of recent conversation kept verbatim),
`precompute` (summarize in the background as context nears the threshold; not implemented in moco).

Triggers: `auto` (estimate over threshold), `overflow` (provider rejected context), `manual` (`/compact`).

Method: ask the model (the `compaction` hidden agent, no tools) to summarize everything before a cut point, keep the recent tail verbatim, store a
compaction message referencing the kept range. Subsequent compactions *update* the existing checkpoint rather than restarting.
If the reply does not follow the template, send one nudge:
`The previous response did not fill in the required summary template. Do not call tools. Return the summary as text using the exact section headings from the template.`

Summary template the model must fill (sections may be omitted when not applicable):

```
## Objective            - one or two brief sentences
## Requirements         - constraints/preferences/scope stated by the user, or (none)
## Decisions            - decisions already made and why, or (none)
## Work State           ### Completed / ### Active / ### Blocked
## Next Move            1. ordered next actions, or (none)
## Relevant Files       at most 15, most important first, `path`: reason
## Important Context    facts the next agent cannot recover on its own, or (none)
```

Rules sent with it: terse single-line bullets; prefer short references; preserve exact paths, symbols, commands, error strings, URLs, identifiers;
carry forward only unanswered user requests with exact wording; preserve workflow state (uncommitted/committed/pushed/under review/merged);
do not mention the summary process; do not continue the task or call tools; return only the structured summary.
Do not summarize assistant setup (AGENTS.md contents, session ID); those are re-supplied.
Create vs update prompts differ: create = "You MUST summarize the conversation above into a structured summary that will be given to another agent to resume the work."
update = "Update the existing checkpoint ... into one consolidated summary. Newer history always takes precedence ...".
Full text: `compaction.ts` `buildPrompt`, `SUMMARY_TEMPLATE`, `SUMMARY_RULES`.

## Subagents (`tool/plugin/subagent.ts`)

Tool `subagent {agent, description, prompt, model?, sessionID?, background?}`:
- Creates a **child session** (`parent_id`) with the chosen agent and fresh context; the parent's `prompt` is its only input.
- Foreground: parent step waits and gets the child's final text plus the child `sessionID` (so the parent can send follow-ups by passing it back).
- Background: returns immediately with status `running`; on completion a synthetic message is injected into the parent session and the parent is woken.
- The tool's agent list shows agents as `- <id>: <description>`. Depth limit config: `experimental.subagent_depth`. `general` and `explore` deny further `subagent` calls by permission rule. (Which agent modes are selectable as subagents: check `subagent.ts` lines 280-317 when implementing.)

## Background shell completion

Shell tool with `background:true` returns `Command moved to the background (shell ID: X).\nOutput is streaming to: <file>`; when the process ends a synthetic
message with the result is inserted and the session resumed. Instruct the model not to poll (text in `tool/plugin/shell.ts` `BACKGROUND_INSTRUCTION`).

## Title and summary generation

First user message triggers the hidden `title` agent (no tools) to produce a 3-5 word title. After a run, the `summary` agent can write a PR-style 2-3 sentence summary.
The prompts are in `crates/moco-core/src/agent.rs` and `title.rs`.

## Revert (undo)

`revert.stage` marks a point (message/part) and restores files from the snapshot taken then; `revert.clear` re-applies; `revert.commit` makes it permanent and drops later messages.
Snapshots are content-addressed trees of the working tree with `capture`, `files`, `diff` and `restore` operations (interface in `core/src/snapshot.ts`; I did not read the implementation, so the storage mechanism is your choice, a private git dir is the obvious one). Best effort: capture returns nothing if disabled or it fails.

## moco implementation notes (Phase 2, compaction slice)

Code: `compaction.rs` (pure logic, prompts) and `engine/compact.rs` and `engine/step.rs` (`compact_inner`, `prepare`, `run_step_inner`).
- **Storage**: a `compaction` message `{summary, keepFrom, trigger, replaced}`. Nothing is deleted. The model's view of history is `compaction::effective`: after the latest compaction, `[checkpoint as a synthetic user message, messages from keepFrom on]`. The checkpoint merges with the next user message into one request message.
- **Cut point**: the longest suffix of the effective history within `keep` tokens (default 20,000, estimated at 4 characters per token), always including the last message. Any message boundary is allowed, because an assistant message holds its tool calls and results together. Nothing happens if everything already fits, or if only the existing checkpoint precedes the tail.
- **Triggers**: `auto` before a step when the prompt size passes `limit - max(buffer, min(16k, limit/2))` (default buffer 15%; the size is the provider-reported total of the previous step plus an estimate of what followed, or an estimate of the whole request when nothing has been reported); `overflow` when the provider rejects the prompt before producing anything (compact once, then retry the step; a second overflow in the same step is surfaced); `manual` via `/compact`. Auto needs a known context limit from the catalog. If auto compaction fails, it is not retried for the rest of that run and the step proceeds.
- **Summarization call**: same model as the session, no tools, system prompt `compaction::SYSTEM`, the history before the cut plus a final user message holding the template and rules (create, or update when a checkpoint exists). Retried with the normal backoff. If the reply has fewer than three of the template headings it gets one nudge. If the portion does not fit the window it is trimmed from the oldest end (up front when the limit is known, by a quarter per overflow otherwise). Its cost is added to the session total.
- **Deviations**: the update prompt wording and the rules text are my own phrasing of the spec's description (the upstream strings were not available); `precompute` and a separate compaction model are not implemented; a `compaction` config section accepts `auto`, `buffer` (tokens or `"15%"`) and `keep`.

## moco implementation notes (Phase 2, snapshots slice)

Code: `snapshot.rs` (the store) and `engine/undo.rs` (`take_snapshot`, `undo`, `redo`, `commit_pending`, `session_diff`, `prune_snapshots`).
- **Store**: not a private git dir (the option the spec suggests) but a small content-addressed store under `<app dir>/data/snapshots/<project id>/`: SHA-256 blobs plus one manifest per snapshot (`path -> hash, size, mode, mtime/ctime hint`). The snapshot id is the hash of the file list, so identical trees share an id. No external `git` is needed, and it behaves the same on Linux and the Mojave machine. Capture walks the directory honoring `.gitignore`/`.ignore` files, includes hidden files, skips `.git`, symlinks and files over 2 MiB, and fails with a notice (snapshots off for that session) beyond 50,000 files, so a home directory is never swept up. Files whose size, mtime and ctime match the previous snapshot are not re-read, unless they changed too close to that snapshot for the timestamps to tell a later edit apart (within 2 s for whole-second timestamps such as HFS+, 100 ms otherwise; archive#97).
- **When**: a snapshot is taken synchronously before every step (the spec takes it in parallel with the request, before tools run) and stored as `snapshot` on that step's assistant message, so it is the state before the step's tools ran. Subagent sessions snapshot too, using the parent's last snapshot as the hash cache. Config `snapshots: false` turns it all off.
- **Undo**: `undo` takes the latest user turn not yet hidden, restores the working tree to the snapshot of that turn's first step, and stores `{messageId, redo}` in the session (`revert` column). The first undo also records the pre-undo state as `redo`; further undos go back one turn each. Messages from `messageId` on are hidden by the marker, not deleted. `redo` restores the `redo` snapshot and clears the marker. The next `run` (any new prompt) or `compact` calls `commit_pending`, which deletes the hidden messages for good. A turn without a snapshot (older sessions, snapshots off) is rolled back in the conversation only, and the UI says so. Restore writes changed files (mode included), deletes files that did not exist at the snapshot, removes directories that become empty, and never touches ignored files or paths outside the directory (manifest paths are validated).
- **Diff**: `session_diff` compares the session's first snapshot with a fresh capture of the tree, so it includes edits made by hand and by shell commands, not just by the edit tools. Unified diffs come from the `similar` crate; binary and non-UTF-8 files are listed without a patch.
- **Pruning**: at startup, manifests older than 30 days that no session active within 30 days references (and a pending `redo`) are deleted, then orphaned blobs.
- **Limits**: only the session's directory is covered; files edited elsewhere (after an `external_directory` approval) and side effects such as installed packages or network calls are not undone. Undo restores the whole tree to the snapshot, so a file you created yourself after that point is removed too until you `/redo` (it stays recoverable from the snapshot store until pruned). Undo is per turn, not per file or per message. Not implemented: per-turn diff views, selecting which files to revert.

## moco implementation notes (Phase 3, waking on background completion)

The spec has a finished background job insert a synthetic message and *resume the session*. moco queues the message as a steer and resumes the session only where that cannot interfere with you:
- while a turn is running, the message is delivered at its next step boundary, so the model reacts mid-turn;
- while the TUI is idle, the session runs again at once so the assistant can react (archive#150). If a picker, viewer or dialog is open, it runs when that closes; text you are typing stays in the composer. `"jobs": {"auto_resume": false}` turns this off, and then the line-based chat's behavior applies;
- while the line-based chat is idle at its prompt, a dim notice line is printed and the message waits; it is delivered with your next message, or immediately if you press Enter on an empty line (the notice says so);
- with `moco -p`, moco waits for running jobs after the turn, runs the session again so the assistant can report, and stops any jobs left when it exits.
Running the session on its own while you are typing a line (the TUI does this cleanly) would interleave its output with your input and could steal your answers to permission prompts, so it is deliberately not done in the line-based chat.

