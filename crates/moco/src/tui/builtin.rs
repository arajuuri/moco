//! The built-in slash commands of the full-screen UI, and the transcript of a stored session.
use super::app::{App, PickItem, PickKind, Picker, Row, ToolState};
use super::Host;
use moco_core::engine::{DiffResult, Engine, UndoReport, UndoResult};
use moco_core::permission::PermissionMode;
use moco_core::store::{Message, Session};

#[allow(clippy::large_enum_variant)]
/// What the event loop must do after a built-in command.
pub enum Effect {
    None,
    /// Continue in another session (new, resumed, forked).
    Switch(Session),
    /// The conversation changed underneath the transcript (undo, redo): rebuild it.
    Reload,
    /// Summarize older messages; slow, so the loop runs it on a worker.
    Compact,
    /// A prompt was queued; start a run if none is going.
    Enqueued,
}

pub const NAMES: &[(&str, &str)] = &[
    ("model", "pick the model"),
    ("agent", "pick the agent"),
    ("mode", "ask before acting (manual) or run tools without asking (auto); Tab switches"),
    ("effort", "set the reasoning effort"),
    ("sessions", "list and resume sessions"),
    ("new", "start a new session"),
    ("undo", "undo the last turn's file changes and messages"),
    ("redo", "bring an undone turn back"),
    ("diff", "changes in the working tree since the session started"),
    ("compact", "summarize older messages to free up context"),
    ("fork", "copy this conversation into a new session"),
    ("rename", "name this session"),
    ("export", "write this conversation to a Markdown file"),
    ("archive", "hide a session from the list"),
    ("theme", "pick a color theme"),
    ("queue", "send a prompt after the current turn, or manage waiting prompts"),
    ("jobs", "background commands and subagents"),
    ("permissions", "what the classifier allowed automatically, and the saved always-allow rules"),
    ("skills", "list skills"),
    ("commands", "list custom commands"),
    ("mcp", "MCP servers"),
];

fn human(n: u64) -> String {
    crate::human(n)
}

fn files(app: &mut App, r: &UndoReport) {
    for f in &r.restored {
        app.push_info(format!("  restored {f}"));
    }
    for f in &r.deleted {
        app.push_info(format!("  removed {f}"));
    }
    // A modified file is listed twice (old and new version), so count paths.
    let n = r.skipped.iter().map(|s| s.path.as_str()).collect::<std::collections::BTreeSet<_>>().len();
    if n > 0 {
        app.push_info(format!("{n} file(s) could not be restored (too large or unreadable)"));
    }
}

/// Switch the session's permission mode, from `/mode` or Tab, and say what it now means.
pub fn set_permission_mode(engine: &Engine, sid: &str, app: &mut App, mode: PermissionMode) {
    engine.store.lock().unwrap().set_permission_mode(sid, Some(mode.name())).ok();
    app.permission_mode = mode;
    app.push_info(format!("permission mode: {} ({})", mode.name(), mode.describe()));
}

fn err(app: &mut App, msg: impl Into<String>) {
    app.rows.push(Row::Error(msg.into()));
}

fn model_items(host: &Host, current: &str) -> Vec<PickItem> {
    let connected = host.engine.connected_providers();
    let mut out = vec![];
    for (pid, p) in &host.engine.catalog().providers {
        if !connected.contains(pid) {
            continue;
        }
        for m in p.models.values().filter(|m| m.tool_call != Some(false)) {
            let full = format!("{pid}/{}", m.id);
            let price = m.cost.as_ref().map_or("no price".to_string(), |c| format!("${}/{} per M", c.input, c.output));
            let now = if full == current { "  (current)" } else { "" };
            out.push(PickItem { detail: format!("ctx {}  {price}  {}{now}", human(m.limit.context), m.name), label: full.clone(), value: full });
        }
    }
    out
}

fn session_items(host: &Host, all: bool, archived: bool) -> Vec<PickItem> {
    let dir = host.cwd.as_str();
    let list = host.engine.store.lock().unwrap().list_sessions(if all { None } else { Some(dir) }, archived, 100).unwrap_or_default();
    list.iter()
        .map(|s| PickItem {
            label: s.title.clone().unwrap_or_else(|| "(untitled)".into()),
            detail: format!("{}  {}  {} msgs  ${:.2}{}", crate::short(&s.id), crate::ago(s.time_updated), s.messages, s.cost, if all { format!("  {}", crate::tilde(&s.directory)) } else { String::new() }),
            value: s.id.clone(),
        })
        .collect()
}

pub fn set_theme(host: &Host, app: &mut App, name: &str) {
    match super::theme::load(name, &host.app_dir, std::path::Path::new(&host.cwd), super::theme::truecolor()) {
        Ok(t) => {
            app.theme = t;
            app.push_info(format!("theme: {name}"));
        }
        Err(e) => err(app, e),
    }
}

pub fn queue_items(app: &App) -> Vec<PickItem> {
    app.queued
        .iter()
        .map(|q| PickItem { label: crate::clip(q.text.lines().next().unwrap_or(""), 60), detail: if q.steer { "next step".into() } else { "after the turn".into() }, value: q.id.clone() })
        .collect()
}

pub fn palette_items(host: &Host) -> Vec<PickItem> {
    let mut v: Vec<PickItem> = vec![];
    let mut add = |name: &str, desc: &str, run: bool| v.push(PickItem { label: format!("/{name}"), detail: desc.to_string(), value: format!("{}/{name}{}", if run { "run:" } else { "ins:" }, if run { "" } else { " " }) });
    for (n, d) in NAMES {
        // Commands that need an argument are inserted into the composer; the rest run at once.
        add(n, d, !matches!(*n, "rename" | "export" | "fork" | "archive" | "mcp" | "queue"));
    }
    add("help", "show the keys", true);
    add("exit", "quit", true);
    for (n, d) in &host.custom_commands {
        add(n, d, false);
    }
    v
}

/// Run built-in command `name`. `None` means it is not a built-in.
pub fn run(host: &Host, app: &mut App, sid: &str, agent: &str, name: &str, args: &str) -> Option<Effect> {
    let engine = host.engine;
    let args = args.trim();
    match name {
        "model" => {
            let connected = engine.connected_providers();
            let current = engine.store.lock().unwrap().session(sid).ok().flatten().map(|s| engine.model_ref_for(&s)).unwrap_or_default();
            if let Some((pid, _)) = args.split_once('/').filter(|(p, _)| connected.iter().any(|c| c == p) && !args.contains(' ')) {
                let known = engine.catalog().model(pid, &args[pid.len() + 1..]).is_some();
                engine.store.lock().unwrap().set_model(sid, Some(args)).ok();
                app.push_info(format!("model: {args}{}", if known { "" } else { " (not in the catalog: no price or context limit)" }));
            } else {
                let items = model_items(host, &current);
                if items.is_empty() {
                    err(app, "no connected provider: set an API key variable or run `moco auth <provider>`");
                } else {
                    app.open_picker(Picker::new(PickKind::Model, "model", items, args));
                }
            }
        }
        "effort" => {
            let Some(sess) = engine.store.lock().unwrap().session(sid).ok().flatten() else { return Some(Effect::None) };
            let info = engine.effort_info(&sess);
            if info.levels.is_empty() {
                app.push_info("this model has no reasoning levels to choose from");
            } else if args == "default" {
                engine.store.lock().unwrap().set_variant(sid, None).ok();
                app.push_info("reasoning effort: default");
            } else if !args.is_empty() {
                if info.levels.iter().any(|l| l == args) {
                    engine.store.lock().unwrap().set_variant(sid, Some(args)).ok();
                    app.push_info(format!("reasoning effort: {args}"));
                } else {
                    err(app, format!("this model offers: {}", info.levels.join(", ")));
                }
            } else {
                let cur = info.desired.clone();
                let mut items = vec![PickItem { label: "default".into(), detail: format!("the provider's own choice{}", if cur.is_none() { "  (current)" } else { "" }), value: "default".into() }];
                items.extend(info.levels.iter().map(|l| PickItem { label: l.clone(), detail: if cur.as_ref() == Some(l) { "(current)".into() } else { String::new() }, value: l.clone() }));
                app.open_picker(Picker::new(PickKind::Effort, "reasoning effort", items, ""));
            }
        }
        "mode" => {
            if args.is_empty() {
                let current = engine.store.lock().unwrap().session(sid).ok().flatten().map(|s| engine.permission_mode_of(&s)).unwrap_or_default();
                app.push_info(format!("permission mode: {} ({}). Tab switches it.", current.name(), current.describe()));
            } else {
                match PermissionMode::parse(args) {
                    Some(m) => set_permission_mode(engine, sid, app, m),
                    None => err(app, "usage: /mode [manual|auto]"),
                }
            }
        }
        "agent" => {
            let selectable: Vec<_> = engine.agents().iter().filter(|a| a.selectable()).collect();
            match selectable.iter().find(|a| a.id == args) {
                Some(a) => {
                    engine.store.lock().unwrap().set_agent(sid, &a.id).ok();
                    app.push_info(format!("agent: {}", a.name));
                }
                None if args.is_empty() => {
                    let items = selectable.iter().map(|a| PickItem { label: a.id.clone(), detail: format!("{}{}", a.description, if a.id == agent { "  (current)" } else { "" }), value: a.id.clone() }).collect();
                    app.open_picker(Picker::new(PickKind::Agent, "agent", items, ""));
                }
                None => err(app, format!("unknown agent; available: {}", selectable.iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join(", "))),
            }
        }
        "sessions" | "resume" => {
            if name == "resume" && !args.is_empty() {
                let found = engine.store.lock().unwrap().find_sessions(args).unwrap_or_default();
                return Some(match found.len() {
                    0 => {
                        err(app, format!("no session matches '{args}'"));
                        Effect::None
                    }
                    1 => resume(host, app, found.into_iter().next().unwrap()),
                    n => {
                        err(app, format!("'{args}' matches {n} sessions; type more of the id"));
                        Effect::None
                    }
                });
            }
            let (all, archived) = (args == "all", args == "archived");
            let items = session_items(host, all, archived);
            if items.is_empty() {
                app.push_info("no sessions");
            } else {
                app.open_picker(Picker::new(PickKind::Session, if archived { "archived sessions" } else if all { "all sessions" } else { "sessions" }, items, ""));
            }
        }
        "new" => {
            let created = engine.store.lock().unwrap().create_session(&host.cwd, agent);
            return Some(match created {
                Ok(s) => Effect::Switch(s),
                Err(e) => {
                    err(app, e.to_string());
                    Effect::None
                }
            });
        }
        "rename" => {
            if args.is_empty() {
                err(app, "usage: /rename <new title>");
            } else if engine.store.lock().unwrap().set_title(sid, args).is_ok() {
                app.title = Some(args.to_string());
                app.push_info("renamed");
            }
        }
        "export" => {
            let path = if args.is_empty() { format!("moco-{}.md", crate::short(sid)) } else { args.to_string() };
            let path = std::path::Path::new(&host.cwd).join(path);
            let text = moco_core::export::export(&engine.store.lock().unwrap(), sid, &moco_core::export::Options::default()).map_err(|e| e.to_string());
            match text.and_then(|t| std::fs::write(&path, t).map_err(|e| e.to_string())) {
                Ok(()) => app.push_info(format!("exported to {} (not redacted: check it for secrets before sharing)", crate::tilde(&path.to_string_lossy()))),
                Err(e) => err(app, format!("export failed: {e}")),
            }
        }
        "fork" => {
            let n =if args.is_empty() { Some(0) } else { args.parse::<usize>().ok() };
            match n {
                None => err(app, "usage: /fork [N]   (N = branch off before the Nth most recent prompt; no N copies everything)"),
                Some(n) => match engine.fork(sid, n) {
                    Ok(Some(f)) => return Some(Effect::Switch(f)),
                    Ok(None) => err(app, format!("this session has fewer than {n} prompt(s)")),
                    Err(e) => err(app, format!("store error: {e}")),
                },
            }
        }
        "archive" | "unarchive" => {
            let unarchive = name == "unarchive";
            let target = if args.is_empty() && !unarchive {
                engine.store.lock().unwrap().session(sid).ok().flatten()
            } else {
                let found = engine.store.lock().unwrap().find_sessions(args).unwrap_or_default();
                if found.len() != 1 {
                    err(app, format!("{} sessions match '{args}'", found.len()));
                    return Some(Effect::None);
                }
                found.into_iter().next()
            };
            if let Some(t) = target {
                engine.store.lock().unwrap().set_archived(&t.id, !unarchive).ok();
                app.push_info(format!("{} {}", if unarchive { "unarchived" } else { "archived" }, crate::short(&t.id)));
                if !unarchive && t.id == sid {
                    if let Ok(n) = engine.store.lock().unwrap().create_session(&host.cwd, agent) {
                        return Some(Effect::Switch(n));
                    }
                }
            }
        }
        "undo" => match engine.undo(sid) {
            Ok(UndoResult::Done(r)) => {
                app.push_info(format!("undone: {} message(s) hidden{}", r.hidden_messages, if r.files_restored { format!(", {} file(s) restored, {} removed", r.restored.len(), r.deleted.len()) } else { "; no file snapshot for this turn, so only the conversation was rolled back".into() }));
                files(app, &r);
                app.push_info("/undo goes further back, /redo brings it all back, sending a message makes the undo permanent");
                return Some(Effect::Reload);
            }
            Ok(UndoResult::NothingToUndo) => app.push_info("nothing to undo"),
            Ok(UndoResult::Busy) => err(app, "session is busy"),
            Ok(UndoResult::Failed(e)) => err(app, format!("undo failed: {e}")),
            Err(e) => err(app, format!("store error: {e}")),
        },
        "redo" => match engine.redo(sid) {
            Ok(UndoResult::Done(r)) => {
                app.push_info(format!("redone: {} file(s) restored, {} removed", r.restored.len(), r.deleted.len()));
                files(app, &r);
                return Some(Effect::Reload);
            }
            Ok(UndoResult::NothingToUndo) => app.push_info("nothing to redo"),
            Ok(UndoResult::Busy) => err(app, "session is busy"),
            Ok(UndoResult::Failed(e)) => err(app, format!("redo failed: {e}")),
            Err(e) => err(app, format!("store error: {e}")),
        },
        "diff" => match engine.session_diff(sid) {
            Ok(DiffResult::Files(d)) if d.is_empty() => app.push_info("no changes since the session started"),
            Ok(DiffResult::Files(d)) => {
                app.viewer = Some(super::app::Viewer {
                    files: d.into_iter().map(|f| super::app::ViewFile { path: f.path, adds: f.additions, dels: f.deletions, binary: f.binary, patch: f.patch }).collect(),
                    scroll: 0,
                });
            }
            Ok(DiffResult::NoSnapshots) => app.push_info("no snapshots yet: nothing has run in this session"),
            Ok(DiffResult::Failed(e)) => err(app, format!("diff failed: {e}")),
            Err(e) => err(app, format!("store error: {e}")),
        },
        "compact" => return Some(Effect::Compact),
        "theme" => {
            if args.is_empty() {
                let items = super::theme::available(&host.app_dir, std::path::Path::new(&host.cwd)).into_iter().map(|n| PickItem { detail: String::new(), label: n.clone(), value: n }).collect();
                app.open_picker(Picker::new(PickKind::Theme, "theme", items, ""));
            } else {
                set_theme(host, app, args);
            }
        }
        "queue" => {
            if args.is_empty() {
                let items = queue_items(app);
                if items.is_empty() {
                    app.push_info("no prompts are waiting (type while a turn runs: Enter steers it into the turn, Ctrl-Q or /queue <text> waits for the end)");
                } else {
                    app.open_picker(Picker::new(PickKind::Queue, "waiting prompts: Enter switches next-step/after-turn, Ctrl-D removes", items, ""));
                }
            } else {
                match (host.submit)(sid, args, moco_core::store::Mode::Queue) {
                    Ok((id, notes)) => {
                        for n in notes {
                            app.push_info(n);
                        }
                        app.queued.push(super::app::Queued { id, steer: false, text: args.to_string() });
                        return Some(Effect::Enqueued);
                    }
                    Err(e) => err(app, e),
                }
            }
        }
        "permissions" => match engine.permissions_report(sid) {
            Ok(lines) => {
                for l in lines {
                    app.push_info(l);
                }
            }
            Err(e) => err(app, e.to_string()),
        },
        "jobs" => {
            if args.is_empty() {
                let jobs = engine.background_jobs(None);
                if jobs.is_empty() {
                    app.push_info("no background jobs");
                } else {
                    let items = jobs.iter().map(|j| PickItem { label: format!("{} {}", j.id, crate::clip(j.title.lines().next().unwrap_or(""), 50)), detail: format!("{} {} {}s", j.kind, j.status, j.seconds), value: j.id.clone() }).collect();
                    app.open_picker(Picker::new(PickKind::Job, "background jobs: Enter shows output, Ctrl-D stops", items, ""));
                }
            } else if let Some(id) = args.strip_prefix("output") {
                match engine.job_output(id.trim(), 40) {
                    Ok(t) => app.rows.push(Row::Shell(t)),
                    Err(e) => err(app, e),
                }
            } else if let Some(id) = args.strip_prefix("kill") {
                match engine.kill_job(id.trim()) {
                    Ok(t) => app.push_info(t),
                    Err(e) => err(app, e),
                }
            } else {
                err(app, "usage: /jobs [output <id> | kill <id>]");
            }
        }
        "skills" => {
            if host.skills.is_empty() {
                app.push_info("no skills found (put SKILL.md folders in .moco/skills, .claude/skills or the config skills dir)");
            }
            for (n, d) in &host.skills {
                app.push_info(format!("  {n:<20} {}", crate::clip(d, 90)));
            }
        }
        "commands" => {
            if let Some(m) = &host.manager {
                for p in m.prompts() {
                    app.push_info(format!("  /{:<18} {}  [mcp]", p.command, crate::clip(&p.description, 70)));
                }
            }
            for (n, d) in &host.custom_commands {
                app.push_info(format!("  /{n:<18} {}", crate::clip(d, 80)));
            }
        }
        "mcp" => {
            let Some(m) = &host.manager else {
                app.push_info("no MCP servers configured (add `mcp.servers` to moco.jsonc)");
                return Some(Effect::None);
            };
            if let Some(n) = args.strip_prefix("reconnect") {
                if m.reconnect(n.trim()) {
                    app.push_info(format!("reconnecting '{}'; /mcp shows its state", n.trim()));
                } else {
                    err(app, format!("no enabled MCP server '{}'", n.trim()));
                }
            } else if let Some(n) = args.strip_prefix("logout") {
                match m.logout(n.trim()) {
                    Ok(true) => app.push_info(format!("forgot the stored sign-in for '{}'", n.trim())),
                    Ok(false) => app.push_info(format!("no stored sign-in for '{}'", n.trim())),
                    Err(e) => err(app, e),
                }
            } else if args.starts_with("auth") {
                err(app, "signing in needs a browser address and a paste prompt: run `moco --plain` and use /mcp auth <name>");
            }
            for i in m.status() {
                app.push_info(format!("  {:<14} {}   [{}]", i.name, moco_core::mcp::manager::describe(&i), crate::clip(&i.command, 50)));
            }
        }
        _ => return None,
    }
    Some(Effect::None)
}

/// Resume a session of this directory.
pub fn resume(host: &Host, app: &mut App, s: Session) -> Effect {
    if s.directory != host.cwd {
        err(app, format!("that session belongs to {}; start moco there, or use `moco --session {}`", crate::tilde(&s.directory), crate::short(&s.id)));
        return Effect::None;
    }
    Effect::Switch(s)
}

/// Transcript rows for a stored conversation. Messages hidden by a pending undo are left out.
pub fn history_rows(messages: &[Message], hidden_from: Option<&str>) -> Vec<Row> {
    let mut rows = vec![];
    for m in messages.iter().filter(|m| hidden_from.is_none_or(|h| m.id.as_str() < h)) {
        match m.kind.as_str() {
            "user" => {
                if let Some(t) = m.data["text"].as_str().filter(|t| !t.is_empty()) {
                    rows.push(Row::User(t.to_string()));
                }
            }
            "synthetic" => {
                if let Some(t) = m.data["text"].as_str().filter(|t| !t.is_empty()) {
                    rows.push(Row::Info(format!("[{}]", t.lines().next().unwrap_or(""))));
                }
            }
            "compaction" => rows.push(Row::Info("(earlier messages were summarized)".into())),
            "assistant" => {
                for p in m.data["parts"].as_array().into_iter().flatten() {
                    match p["type"].as_str() {
                        Some("text") => {
                            if let Some(t) = p["text"].as_str().filter(|t| !t.trim().is_empty()) {
                                rows.push(Row::Assistant(t.to_string()));
                            }
                        }
                        Some("tool") => {
                            let name = p["name"].as_str().unwrap_or("").to_string();
                            let state = match p["state"]["status"].as_str() {
                                Some("completed") => ToolState::Done,
                                Some("error") => ToolState::Failed(p["state"]["error"].as_str().unwrap_or("error").to_string()),
                                _ => ToolState::Failed("interrupted".into()),
                            };
                            let output = super::diff::output_head(p["state"]["output"].as_str().unwrap_or(""), 12);
                            rows.push(Row::Tool { id: p["callID"].as_str().unwrap_or("").to_string(), depth: 0, agent: String::new(), summary: crate::summary(&name, &p["input"]), detail: super::diff::tool_detail(&name, &p["input"]), output, name, state, auto: None });
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(id: &str, kind: &str, data: serde_json::Value) -> Message {
        Message { id: id.into(), session_id: "s".into(), kind: kind.into(), data }
    }

    #[test]
    fn stored_conversations_become_rows_without_the_undone_part() {
        let ms = vec![
            msg("m1", "user", json!({"text": "read it"})),
            msg("m2", "assistant", json!({"parts": [
                {"type": "text", "text": "Sure."},
                {"type": "tool", "name": "read", "input": {"path": "a.rs"}, "state": {"status": "completed", "output": "x"}},
                {"type": "tool", "name": "read", "input": {"path": "b.rs"}, "state": {"status": "error", "error": "no such file"}},
                {"type": "native", "provider": "p", "data": {}}]})),
            msg("m3", "synthetic", json!({"text": "job done\nmore"})),
            msg("m4", "user", json!({"text": "undone prompt"})),
            msg("m5", "assistant", json!({"parts": [{"type": "text", "text": "undone answer"}]})),
        ];
        let rows = history_rows(&ms, Some("m4"));
        assert_eq!(rows.len(), 5);
        assert!(matches!(&rows[0], Row::User(t) if t == "read it"));
        assert!(matches!(&rows[1], Row::Assistant(t) if t == "Sure."));
        assert!(matches!(&rows[2], Row::Tool { name, summary, state: ToolState::Done, .. } if name == "read" && summary == "a.rs"));
        assert!(matches!(&rows[3], Row::Tool { state: ToolState::Failed(e), .. } if e == "no such file"));
        assert!(matches!(&rows[4], Row::Info(t) if t == "[job done]"));
        assert_eq!(history_rows(&ms, None).len(), 7);
    }
}
