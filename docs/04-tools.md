# Built-in tools

Sources: `packages/core/src/tool/plugin/*.ts`, `tool/read-filesystem.ts`, `tool/patch.txt`, `tool/todowrite.txt`, `file-access.ts`, `tool-output.ts`.
Model-facing names are lowercase. Input fields are camelCase. Each tool gets: name, description (model-visible), JSON-schema input, execute.
Descriptions below are paraphrased except where quoted; copy exact text from the source file when you want identical model behavior,
because tool descriptions steer the model strongly.

Common rules
- Paths may be relative to the session working directory (the "Location") or absolute.
- A path inside the Location or its project root is *internal*. Anything else is *external*: it additionally requires an `external_directory` permission
  with resource `<dir>/*` (save pattern = project-root-of-that-dir `/*`). See `05-permissions.md`.
- Every result is bounded by the generic output truncation (2000 lines / 50 KiB, spill to file) in addition to tool-specific limits.
- Tool failures that the model can act on return a `ToolFailure {message}`; defects/interrupts propagate.

## read
Input `{path: string, offset?: int (1-based line or dir entry), limit?: int (default and max 2000)}`.
- Text file: lines prefixed `<line>: <content>`. Lines over 2000 chars are cut with `... (line truncated to 2000 chars)`.
- Directory: entries one per line (offset/limit apply to entries).
- Images and PDFs are passed to the model as media (subject to a size limit and optional auto-resize, config `media.image.{auto_resize,max_width,max_height}`).
- Binary files error. Offset beyond end errors.
- If the path is missing, try a fuzzy sibling match: same directory, unique file whose name is equal after NFC normalization, `U+00A0`/`U+202F` -> space, curly quotes -> `'` (macOS screenshot names). Else error `File not found`.
- Permission: `read` with resource = path relative to Location (or absolute if external); `.env` rules in the default ruleset.
- Side effect: after a successful read, walk up from the file's directory to (excluding) the Location root and load any `AGENTS.md` found as durable instructions for the session.
- Description (verbatim intent): "Read the contents of a file or directory. Supports text files, images, and PDFs... Prefer one larger read over many small slices, and use grep to find specific content in large files."

## write
Input `{path, content}`. Creates parent directories, overwrites existing. Output records `existed`. Permission action `edit` (shared by write, edit, patch), resource = relative path, save `*`. Preserves an existing BOM, runs the project formatter on the file if one applies, and records a file diff. Description: "Writes a file to the local filesystem, overwriting if one exists. Missing parent directories are created automatically. ... For partial changes, use the edit tool instead."

## edit
Input `{path, oldString, newString, replaceAll?: bool}`. Rules:
1. `newString` must differ from `oldString`.
2. Exact match must occur once, unless `replaceAll`. Zero matches -> error "not found"; many matches -> error telling the model to add context or use replaceAll.
3. If no exact match, retry with relaxed matching in order: (a) ignore typographic quote/dash/space variants, (b) ignore trailing whitespace per line.
4. Preserve the file's line endings: model writes `\n`; if the file is CRLF, convert. To change line endings, make old/new differ only in `\r` (exact-bytes edit).
5. Preserve BOM (`util/bom.ts`); permission `edit`, resource = relative path, save `*`; formatter runs afterward.
6. Output `{files: FileDiff[], replacements: n}`.
The model is told not to include the `1: ` line-number prefix from `read` output.

## patch
Input `{patchText}`. Grammar (also the tool description):
```
*** Begin Patch
*** Add File: <path>        then every line prefixed '+'
*** Delete File: <path>
*** Update File: <path>     optional '*** Move to: <new path>', then hunks starting '@@ [context line]' with ' ', '-', '+' lines
*** End Patch
```
Applies atomically: validate all hunks first (`before`/`after` content computed per file), then write. Output `{applied: [{type: add|update|delete, resource, target}], files}`.
Model content: `Success. Updated the following files:` then lines `A path` / `M path` / `D path`. Permission action `edit`.
Parser lives in `packages/util/src/patch.ts` (read it; it also defines fuzzy context matching).

## glob
Input `{pattern, path?, hidden?: bool (default false), limit?: int (default 100)}`. Returns file entries (path, type). Matching goes through `core/src/ripgrep.ts` and `filesystem.ts`; read those for ignore-file behavior (not verified here). Description: 'Search file paths using a glob pattern (examples: "**/*.ts", "src/**/*.tsx").' Permission `glob`, resource = the pattern, save `*`; the search directory is checked as `external_directory` first. The literal strings `"undefined"`/`"null"` for `path` are treated as unset (models emit them).

## grep
Input `{pattern, path?, include?, literal?: bool (default false), caseSensitive?: bool (default true), limit?: int (default 100)}`. Ripgrep regex syntax or literal. Returns matches (path, line number, line preview). Permission `grep`, resource = the pattern, save `*`; search directory checked as `external_directory` first.

## shell
Input `{command, workdir?, timeout?: ms, background?: bool}`.
- Default timeout 120000 ms foreground; `0` disables; background has none by default.
- Shell chosen by config `shell` or login shell (`/bin/zsh` on 10.14 is fine; bash 3.2 lacks features).
- Before running: scan the command with a shell parser to extract (a) commands, each yielding a permission resource and a "save" pattern, and (b) directories touched by `cd` and path flags, which are checked as external directories. Permission action `shell` (details in `05-permissions.md`).
- Output: combined stdout+stderr streamed to a file; the result returns a bounded tail with the full-output path when truncated. Structured output `{output, exit?, signal?, shellID?, truncated, timeout?, status: completed|running}`.
- Notices appended to output for: timeout, signal, non-zero exit (see `core/src/shell/result.ts`).
- Background: returns immediately with `Command moved to the background (shell ID: X).\nOutput is streaming to: <file>`; completion injects a synthetic message and wakes the session.
- Env: inherit; set `TERM=dumb`-like non-interactive; no TTY unless user opens a terminal.
- User-initiated `!command` input in the UI uses the same machinery without the model (`session.shell`).

## webfetch
Input `{url, format?: "text"|"markdown"|"html" (default markdown), timeout?: seconds (default 30, max 120)}`.
- Only http/https. Redirects are followed manually; each redirect target is re-checked against permission rules (`assertNotDenied`).
- Response size cap 5 MiB (`MAX_MARKDOWN_BYTES` in `tool/html-markdown.ts`; error "Response too large (exceeds N byte limit)").
- HTML -> markdown conversion (`tool/html-markdown.ts`); `text` format is a stripped version.
- User-Agent: `Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko); compatible; OpenCode-User/1.0; +https://opencode.ai` (change this in your app).
- Permission `webfetch` with resource = URL, save `*`.
- Output `{url, contentType, output}`.

## websearch
Input `{query}`. Provider-pluggable (config `websearch.provider`); first use asks the user whether to allow web search and which provider. Output list of `{title, url, snippet...}`. Optional; implement last.

## todowrite
Input `{todos: [{content, status: pending|in_progress|completed|cancelled}]}`. Replaces the session's list wholesale; UI shows it in the sidebar. Description (verbatim, `tool/todowrite.txt`):
"Create and update a todo list for the current session. The user sees it as your working plan. Use it proactively when a task has 3 or more distinct steps... Send the full list on every call; it replaces the previous one. Mark items `in_progress` when you start them and `completed` as soon as they're actually done. Only call it when the list changes."

## question
Input `{questions: [{question, header (<=30 chars), options: [{label (1-5 words), description}], multiple?: bool}] (non-empty)}`. Pauses the run and shows a form; output `{answers: string[][]}` (one array of chosen labels per question; free text allowed as "Other"). User dismissal raises a cancellation that stops the step. Allowed for `build` and `plan` agents; denied for `general`.

## skill
Input `{id}`. Loads a skill (see `08-config-and-extensions.md`). Output is a `<skill_content>` block: `# Skill: <name>`, the body, `Base directory for this skill: <dir>`, a note that relative paths resolve there, and up to 10 sibling files in `<skill_files>`.

## subagent
See `03-agent-loop.md`. Input `{agent, description (3-5 words), prompt, model?, sessionID?, background?}`; output `{sessionID, status: completed|running, output}`.

## MCP tools
Each tool of each connected MCP server becomes a tool named `<server>_<tool>` (sanitized). Input schema is the server's JSON schema; results map text/image content blocks to tool content. `mcp-resource` tool reads MCP resources by URI. Permission action = the tool's full name.

## Optional / later
`schedule` (cron-style recurring agent runs), `image_generate`, `artifact`, `opencode` (introspection). Skip.

## Tool registry behavior to replicate
- Registrations are ordered; later registration of the same name wins; removing it reveals the earlier one.
- A tool is *advertised* to the model unless the agent's permissions deny its action with `*` resource (catalog filtering). A call still goes through its own permission check.
- Each model request captures the tool list at request time.

## moco implementation notes (Phase 2, tools slice)

Code: `crates/moco-core/src/tools/` (`patch.rs`, `webfetch.rs`, `html.rs`, `todowrite.rs`, `question.rs`, `subagent.rs`) and `engine.rs` (`EngineHost`).
- **Host interface**: tools that need engine state use `ToolCtx::host` (`set_todos`, `ask`, `subagent`), implemented by the engine; tests use a stub. `Tool::description` returns a `String` because the subagent tool lists the available agents.
- **patch**: the `*** Begin Patch` grammar from above (`Add File` lines need `+`; `Update File` with optional `Move to`, `@@ context` hunks, `*** End of File`). Matching tries exact, then trailing-whitespace-insensitive, then trim-both, then typographic-variant equality; unchanged context lines keep the file's own text. Everything is staged in memory first (later operations see earlier ones), permission is one `edit` request listing every touched path, and nothing is written if any hunk fails. Add fails if the file exists; Move fails if the destination exists. CRLF, BOM and a missing trailing newline are preserved. Not atomic across a crash during the final write loop.
- **webfetch**: http/https only, redirects followed manually (max 5) with a `webfetch` permission check on every hop, 5 MiB cap (also checked against `Content-Length`), text-like content types only. HTML goes through a small built-in converter (`html.rs`: headings, paragraphs, lists, links, code and `pre`, quotes, images, simple tables; scripts, styles and head skipped), not a full HTML parser. Non-HTML text (JSON, plain, Markdown) is returned unchanged. The User-Agent is `Mozilla/5.0 (compatible; moco/0.0; ...)`.
- **todowrite**: replaces the list in the `todo` table, at most one `in_progress`, and the terminal prints it as a checklist.
- **question**: the form goes to `Ui::ask_questions`; one answer list per question (option labels or free text). Dismissing the form (`None`) is treated like a rejected call and stops the run. Prompts from several sessions never interleave (one process-wide prompt lock).
- **subagent**: creates a child session (`parent_id`) under the same project and directory, with the parent's model unless `model` is given. It runs synchronously inside the tool call; the child's tool activity and cost appear in the parent's UI as `Nested` events, its permission prompts go to the same user, and a rejection inside the child stops the whole run. The result is `sessionID: <id>` followed by the child's last assistant text; passing that ID continues the same session (it must be a child of the calling session). Depth is limited to 3. Usage rolls up to ancestor sessions. Not implemented: `background: true`.
- **Agents**: `general` (subagent mode, `question` and `subagent` denied) and `explore` (subagent mode, `* deny` then read/grep/glob/shell/webfetch allowed, `.env` rules kept, `external_directory` allowed, upstream's search-specialist prompt) were added. Agents have a mode (`primary`, `subagent`, `all`); config-defined agents default to `all` and `mode` can be set in config. Only non-subagent agents can be chosen with `--agent` or `/agent`. The base prompt plus eleven tool definitions is about 3.6k tokens per request.
- **Still missing**: `skill`, MCP tools, `websearch`, `schedule`, background shell.

## moco implementation notes (Phase 3, images and PDFs)

Code: `media.rs`, `blobs.rs`, `tools/read.rs`, `mentions.rs`, `chat.rs`.
- **`read`** returns images (PNG, JPEG, GIF, WebP) and PDFs to the model as media, recognized by their bytes (magic numbers), not their file name, so a `.png` that is not an image is an ordinary binary file and a PDF without an extension still works. The tool output says what was attached and the content follows the tool result in the same user message.
- **Limits** (`media::check`) are enforced before anything is stored or sent, because an image the API rejects would be resent, and rejected again, on every later step of the conversation: images up to 5 MiB and 8000 px per side (read from the PNG, JPEG, GIF or WebP header), PDFs up to 10 MiB. The error says how to fix it (on macOS `sips -Z 2000 file.png --out small.png`). There is no automatic resizing; spec's `media.image.*` config is not implemented.
- **Model support**: the catalog's `modalities.input` decides whether the current model takes images or PDFs (unknown means yes). A model without support gets no attachment: `read` returns an error naming the problem and `@file` prints a note.
- **Storage**: attachments are content-addressed files in `<app dir>/data/blobs/` (`blobs.rs`); messages hold `{mediaType, blob, size, name}` references (user messages under `mentions.files[]` with `kind: "media"`, tool results under `state.media`). History stays small, forks share the files, and a deleted blob degrades to a text note instead of failing a request. Blobs are not yet pruned. Context estimates count an attachment by its size (about 1,500 tokens per image, `size/12` for a PDF) instead of its reference JSON.

