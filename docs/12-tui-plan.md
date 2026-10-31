# Full-screen TUI: plan

Status: planned. The feasibility check (`spike-tui/`) is done; nothing of the TUI itself is built yet. Screen concepts come from `09-ui.md`; this file decides how moco builds them.

## Where we stopped

- Phase 3 is committed through slice G (MCP over Streamable HTTP with OAuth, `553deef`); the plan and spike are `14ef6ee`. Working tree clean. 300 unit + 2 protocol tests pass on Linux and on the Mojave machine.
- Next step: slice 1 (skeleton) below. Not started; the user had not yet said "go".
- Open items: the manual Terminal.app/iTerm2 look-and-feel pass is still to do; OpenAI Responses, Gemini and MCP HTTP/OAuth remain verified only against local fakes.
- Mac scratch: `~/moco-target` and `~/spike-tui-target` build dirs (fine to keep); temp driver files were removed.

## Feasibility result (spike-tui)

ratatui 0.30 + crossterm build on the Mojave machine with `MACOSX_DEPLOYMENT_TARGET=10.14` (`minos 10.14`, links only `libSystem` and `libiconv`). Run in a pty over SSH with `TERM=xterm-256color`, driven by a script: alternate screen enter/leave, key events, arrows/Backspace/Delete, bracketed paste as one event, SIGWINCH resize with redraw, clean exit. Not checked: how it looks in Terminal.app or iTerm2 (colors, mouse, wide characters). Terminal.app has no kitty keyboard protocol, so Shift-Enter cannot be told from Enter: newline in the composer is Alt-Enter (and Ctrl-J).

## Decisions

- **Library**: ratatui + crossterm, in `crates/moco` behind a new `tui/` module. The line-based chat stays as the fallback (`--plain`, and automatically when stdout is not a terminal), so scripts, `-p` and the protocol tests keep working.
- **Threads, no async**: the UI thread owns the terminal and an `mpsc` channel of `AppEvent` (key, paste, resize, engine event, tick). A reader thread forwards crossterm events; the engine runs on a worker thread per run and its `Ui` implementation (`TuiUi`) turns `UiEvent`s into `AppEvent`s. `Ui::ask` and `ask_questions` block the worker on a reply channel while the UI thread shows the dialog.
- **State is a model, rendering is pure**: `App` (transcript rows, composer, dialog stack, run state, usage) with `update(AppEvent)` and `render(&App, Frame)`. Tests drive `update` with scripted events and render into ratatui's `TestBackend` to assert on the buffer; no terminal needed on Linux.
- **Theme and keys are data**: a `Theme` of semantic roles and a `Keymap` of action names, both loaded from config (`tui.theme`, `tui.keys`), so the Phase 3 "themes, keybinding config" item is part of this work rather than separate.
- **No mouse capture by default**, so the terminal's own selection and copy keep working over SSH; scrolling is by keys (PageUp/PageDown, Ctrl-Home/End).

## Slices

1. **Skeleton**: terminal setup/teardown (also on panic and SIGTERM), event loop, `App`/`render` split, transcript of text and tool rows, one-line composer, run on a worker thread, Ctrl-C/Esc interrupt, status line (model, agent, cost, context fill). Streaming text appended to the last row; follow-tail with manual scroll. Replaces plain streaming for `moco` with no arguments. Mac check in a pty.
2. **Composer**: multiline editing (Alt-Enter newline, word moves, kill, history), bracketed paste (large pastes summarized), `@` completion (files, skills, agents, MCP resources) and `/` command completion, `!command`.
3. **Dialogs**: permission prompt (once / always / reject, with the command or diff shown), question form, model picker, session list (resume, rename, fork, archive), command palette (`Ctrl-P`), help.
4. **While running**: send during a run as steer (default) or queue (the engine already supports both), a queued-prompts panel, background jobs panel (`/jobs`), subagent rows indented and collapsible, todo checklist, retry banner with countdown.
5. **Rendering quality**: Markdown for assistant text (headings, lists, code blocks, tables), collapsible reasoning, inline diffs for `edit`/`write`/`patch`, `/diff` viewer pane, verbosity cycling, wide-character and tab handling, bounded redraw cost on long transcripts (render only the visible window).
6. **Polish**: themes (dark, light, custom file; background color is never assumed, no OSC 11 query needed), keybinding config with the upstream leader scheme (`Ctrl-X`) as the default, terminal bell and title when a run ends or asks permission while unfocused, optional sidebar (context, MCP, todos, jobs) when the terminal is wide enough.

## Acceptance checks

- `cargo test` covers `update`/`render` with `TestBackend` for each slice (streaming, scroll, dialog answers, queue/steer, resize), plus the existing suites unchanged.
- A pty-driven script on the Mojave machine per slice (like the spike's) with a scripted fake LLM (`crates/moco/tests/fake_llm.py`): types a prompt, answers a permission dialog, interrupts, resizes, exits; asserts the terminal is restored (no leftover alternate screen or raw mode) even after SIGKILL of the worker and after a panic.
- `otool -l` still shows `minos 10.14`; the binary size and start-up time are noted in the roadmap.
- Manual pass in Terminal.app and iTerm2 on the Mojave screen (colors, wide characters, copy/paste, resize); recorded as done or not done in this file.

## Risks and open points

- Over a slow SSH link, full redraws are costly: rely on ratatui's diffing and avoid animating more than a spinner.
- Terminal.app limits: no truecolor (256 colors only, so themes need a 256-color mapping), no kitty keys, Option-as-Meta must be enabled by the user for Alt shortcuts (Esc-prefix is accepted as Alt).
- The engine's `run` is blocking and `Ui` is called from tool threads; `TuiUi` must never block the UI thread and never hold a lock while waiting for a dialog answer.
- Alternative if ratatui's text handling proves inadequate for the composer: a small custom editor widget (planned anyway in slice 2) rather than a third-party textarea crate.
