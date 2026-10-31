# Agents and prompts

Sources: `core/src/plugin/agent.ts`, `plugin/plan.ts`, `plugin/optimize.ts`, `session/system-prompt.ts`, `session/runner/prompt/system.txt`, `plugin/system-prompt/*.txt`, `schema/src/agent.ts`.

## Agent definition

```
Agent { id, name, model?: {providerID, modelID, variant?}, request {settings, headers, body}, system?: string,
        description?, mode: "primary"|"subagent"|"all", hidden: bool, color?, steps?: int>0, permissions: Rule[] }
```
- Selectable by the user as the session agent: `mode != subagent` and not hidden. Default = config `default_agent`, else `build`, else first selectable.
- `agent.system` replaces nothing; it is added to the base system prompt for that agent.
- `steps` caps steps per run (see MAX_STEPS in `03-agent-loop.md`).
- Config `agents.<id>` can override fields of built-ins or define new ones; `disabled: true` removes one. Custom agents may also be markdown files with YAML frontmatter in `agents/` dirs (see `08`).

## Built-in agents

| id | name | mode | hidden | notes |
|---|---|---|---|---|
| `build` | Build | primary | no | "The default agent. Executes tools based on configured permissions." Adds `question: allow` |
| `plan` | Plan | primary | no | "Read-only agent for exploring the codebase and planning work before implementation." Adds `question: allow`, `edit: deny`, `edit: <plan dir>/*: allow` |
| `general` | General | subagent | no | "General-purpose agent for researching complex questions and executing multi-step tasks. Use this agent to execute multiple units of work in parallel." Adds `question: deny`, `subagent: deny` |
| `explore` | Explore | subagent | no | file search specialist; permissions: `* deny`, then allow `shell, grep, glob, webfetch, websearch, read` (ask for `.env`, `.env.*`; allow `.env.example`), `subagent deny`, `external_directory allow`. System prompt below |
| `compaction` | Compaction | primary | yes | summarizer, prompt built by compaction code |
| `title` | Title | primary | yes | `* deny`; system prompt below |
| `summary` | Summary | primary | yes | `* deny`; system prompt below |

Because last-match-wins, an agent's own rules are appended after the default `* allow` rule, which is why `* deny` followed by specific allows works.

### explore system prompt (verbatim)
```
You are a file search specialist. You excel at thoroughly navigating and exploring codebases.

Guidelines:
- Your role is EXCLUSIVELY to search and analyze
- Parallelize independent tool calls for searches and reads whenever possible
- Adapt your search approach based on the thoroughness level specified by the caller
- Return file paths as absolute paths in your final response
- You MUST NOT create, modify, delete, move, or copy files, including temporary files and reports
- Shell commands MUST be read-only. NEVER run commands that write files or change system state

Complete the user's search request efficiently and report your findings clearly.
```

### summary system prompt (verbatim)
```
Summarize what was done in this conversation. Write like a pull request description.

Rules:
- 2-3 sentences max
- Describe the changes made, not the process
- Do not mention running tests, builds, or other validation steps
- Do not explain what the user asked for
- Write in first person (I added..., I fixed...)
- Never ask questions or add new questions
- If the conversation ends with an unanswered question to the user, preserve that exact question
- If the conversation ends with an imperative statement or request to the user (e.g. "Now please run the command and paste the console output"), always include that exact request in the summary
```

### title system prompt
Full text in `plugin/agent.ts` `PROMPT_TITLE`. Contract: input is the first user message (or, for long conversations, the original request plus recent messages); output is one line, no quotes, no trailing period, no "Title:" label; 3-5 words; names the specific subject; sentence case; preserves identifiers exactly; same language as the message; names the message rather than answering it; greetings -> "Greeting". It includes 7 few-shot examples such as `debug 500 errors in production -> Production 500 errors`.

## Base system prompt (`runner/prompt/system.txt`, verbatim)

```
You are an AI agent running in OpenCode, a coding agent harness. Help the user accomplish their goals using the tools you have available.

# Harness
- Responses are rendered as GitHub-flavored Markdown.
- `<system-reminder>` blocks are harness instructions, not user-authored content. Read and follow them.
- Prefer parallelizing independent tool calls.
${OPENCODE_TOOL_GUIDANCE}

# Communication
- Use clear file paths when referring to files.
- Keep responses clear and concise, and avoid unnecessary technical jargon.

# Working in codebases
- Keep changes consistent with the structure, naming, style, and patterns of the surrounding code.
- Treat unfamiliar files or changes as potential user work and investigate before deleting or overwriting them.
```
`${OPENCODE_TOOL_GUIDANCE}` is replaced by bullet lines depending on which tools are enabled:
- shell present: "Prefer dedicated tools over shell commands; fall back to the shell when a tool cannot do what you need." and "Do not chain shell commands with separators like `echo "===="`; or `printf '---'`; the output becomes noisy in a way that makes the user's side of the conversation worse."
- write present: "Use the write tool to create files or completely replace their content. Prefer using the edit tool for targeted changes."
- edit present: "Use the edit tool for targeted changes to existing text files. It replaces the exact text in `oldString` with `newString`, and the values must differ. By default, `oldString` must occur exactly once. If it occurs multiple times, include more surrounding context to make it unique or set `replaceAll` to true to replace every occurrence."

## Model-specific additions (`plugin/optimize.ts`)

- Model id contains `gpt`: replace the base prompt with `system-prompt/gpt.txt` (or `gpt-astra.txt` if id contains `astra`).
- `claude`: **append** `anthropic.txt` (a short "Code comments" section: match surrounding comment density, comment sparingly; user/project instructions take precedence).
- `kimi`, `trinity`, `muse` (Meta): replace with their own files.
- Upstream also has disabled hooks that delete `grep`/`glob` for GPT/Claude; you may keep all tools.
A rewrite can start with the base prompt only and add per-family prompts when a model misbehaves.

## Environment and instructions context

Not fully read. What I confirmed: instructions (AGENTS.md files, skill list) are tracked in `instruction_state` and sent as *deltas* when they change
(`skill/instructions.ts` renders added/changed/removed skills rather than rewriting history), which keeps the provider prompt cache prefix valid.
I did not find (and did not look for) an injected block of working directory/platform/date/git state; read `session/instructions.ts`, `session/context.ts`
and `session/model-request.ts` before deciding what to include. A sensible default for your app: working directory, OS version, current date, git branch/status.
(`session/environment.ts` is something else: per-session environment variables for shell processes.)

## Plan mode reminders (verbatim, `plugin/plan.ts`)

On entering Plan (session created with plan agent, or switched to it) inject a synthetic user message:
```
<system-reminder>
You are in Plan mode. Discuss the plan with the user directly in the conversation. Do not create or update plan files unless the user explicitly asks you to; when they do, write them only in:
<home>/.opencode/plan

Do not modify any other files or ask a subagent to do so.

You remain in Plan mode until the user switches agents. If the user asks you to implement changes, do not do so. Tell them they need to switch agents.
</system-reminder>
```
On leaving Plan:
```
<system-reminder>
You are NO LONGER in Plan mode. The previous Plan restrictions no longer apply. Any Plan mode instructions from earlier in this conversation are no longer active.
</system-reminder>
```
Re-inject the enter reminder on each request if it is missing (compaction or revert can remove it) near the tail of the context, before the last user message. If an edit/write/patch call in Plan is blocked by permissions, the tool error is rewritten to `Cannot use <tool> to modify files outside the Plan directory: <dir>`.

## Built-in slash commands
- `/init`: template `core/src/plugin/command/initialize.txt` (creates/updates AGENTS.md; uses `$ARGUMENTS`).
- `/review`: template `plugin/command/review.txt`.
- `/compact`: manual compaction (a session operation, `POST /session/:id/compact`, not a template command).
- MCP prompts are exposed as commands named from server and prompt.
Command templates: if the template contains `$ARGUMENTS` replace it with the user's args; otherwise append args after a blank line.
