//! `@` mentions in a prompt (docs/09 "Composer"): `@path` attaches a file, image, PDF or directory listing, `@skill`
//! loads a skill, `@agent` asks the model to delegate to that subagent. Everything is resolved when the prompt is
//! submitted and stored with the message (images and PDFs as blobs), so a conversation replays exactly as it was sent.
//! An `@` only starts a mention at the beginning of a word, so `me@example.com` is left alone. A path may contain
//! spaces if they are escaped with a backslash, which is how a terminal pastes a dragged-in file.
//!
//! Mentions the user typed attach what they name (only a `deny` rule stops them). Mentions in text the user did not
//! write, a project's command file or an MCP server's prompt (`Origin`), get no more than a read the model could make
//! without a prompt: the file must be inside the project (symlinks resolved) and `read` must be allowed without
//! asking. Anything else is left as plain text, which the model can still ask to read with the read tool.
use crate::permission::Effect;
use crate::provider::Block;
use crate::skills::{self, Skill};
use crate::tools::MediaCaps;
use crate::{blobs, media};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_LINES: usize = 2000;
const MAX_TEXT_BYTES: usize = 50 * 1024;
const MAX_DIR_ENTRIES: usize = 200;

pub struct Ctx<'a> {
    pub cwd: &'a Path,
    pub skills: &'a [Skill],
    /// Agents the subagent tool can start.
    pub subagents: &'a [String],
    /// Where attachments are stored (`blobs::put`).
    pub app_dir: &'a Path,
    /// What the current model accepts.
    pub caps: MediaCaps,
    /// MCP servers, for `@server:resource` mentions.
    pub mcp: Option<&'a crate::mcp::Manager>,
    /// How the session's permission rules answer `(action, resources)` without asking (`Engine::decide_now`).
    /// `None` means there are no rules: everything is allowed.
    pub decide: Option<&'a dyn Fn(&str, &[String]) -> Effect>,
}

/// Who wrote the text whose mentions are expanded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// The user: typed it, or ran a built-in command or one from their own config.
    User,
    /// A command the user did not write (a project's `.moco/commands` or `.claude/commands`, a project config's
    /// `commands`). `args` is what the user typed after the command name: a mention of one of those words is theirs.
    Command { args: String },
    /// The text an MCP server returned for one of its prompts. Its own resources may be attached (it could have
    /// inlined them anyway), nothing else beyond what an unprompted read allows.
    McpPrompt { server: String },
}

impl Origin {
    /// Whether the mention `tok` counts as typed by the user.
    fn typed(&self, tok: &str) -> bool {
        match self {
            Origin::User => true,
            Origin::Command { args } => tokens(args).iter().any(|t| t == tok) || args.split_whitespace().any(|w| w == tok),
            Origin::McpPrompt { .. } => false,
        }
    }
    fn label(&self) -> String {
        match self {
            Origin::User => "you".into(),
            Origin::Command { .. } => "a project command".into(),
            Origin::McpPrompt { server } => format!("a prompt of MCP server '{server}'"),
        }
    }
}

/// First `MAX_LINES` lines / `MAX_TEXT_BYTES` bytes of `full` (CRLF normalized): `(kept, truncated, line count, lines kept)`.
fn limit_text(full: &str) -> (String, bool, usize, usize) {
    let full = full.replace("\r\n", "\n");
    let pieces = full.split('\n').count(); // a trailing newline yields an empty last piece that is not a line
    let total = pieces - usize::from(full.ends_with('\n'));
    let (mut kept, mut used, mut n) = (String::new(), 0usize, 0usize);
    for line in full.split('\n') {
        if n >= MAX_LINES || used + line.len() + 1 > MAX_TEXT_BYTES {
            break;
        }
        if n > 0 {
            kept.push('\n');
        }
        kept.push_str(line);
        used += line.len() + 1;
        n += 1;
    }
    (kept, n < pieces, total, n)
}

/// Distinct `@tokens` of a prompt, trailing punctuation removed, `\ ` unescaped, in order of appearance.
pub fn tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (pos, c) = chars[i];
        let starts_word = pos == 0 || chars[i - 1].1.is_whitespace() || matches!(chars[i - 1].1, '(' | '[' | '{' | ',' | '"' | '\'');
        if c == '@' && starts_word {
            let mut j = i + 1;
            while j < chars.len() && (!chars[j].1.is_whitespace() || chars[j - 1].1 == '\\') {
                j += 1;
            }
            let end = chars.get(j).map(|(p, _)| *p).unwrap_or(text.len());
            let raw = text[pos + 1..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"']);
            let tok = raw.replace("\\ ", " ");
            if !tok.is_empty() && !tok.starts_with('@') && !out.contains(&tok) {
                out.push(tok);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

fn resolve_path(tok: &str, cwd: &Path) -> PathBuf {
    let p = match tok.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(tok),
    };
    crate::tools::normalize(&if p.is_absolute() { p } else { cwd.join(p) })
}

fn shown(p: &Path, cwd: &Path) -> String {
    p.strip_prefix(cwd).map(|r| r.display().to_string()).unwrap_or_else(|_| p.display().to_string())
}

/// The resources a `read` of `path` is checked against, built as `ToolCtx::permit_path` builds them: the path as
/// given (relative to the working directory or project when inside) and, when symlinks lead elsewhere, where they
/// lead. A rule such as `read *.env` therefore also catches `notes.txt -> .env`.
fn read_resources(path: &Path, cwd: &Path, root: &Path) -> Vec<String> {
    use crate::tools::{normalize, real_path};
    let name = |p: &Path, bases: [PathBuf; 2]| {
        bases.iter().find_map(|b| p.strip_prefix(b).ok().filter(|r| !r.as_os_str().is_empty()).map(|r| r.display().to_string())).unwrap_or_else(|| p.display().to_string())
    };
    let lexical = normalize(path);
    let mut out = vec![name(&lexical, [normalize(cwd), normalize(root)])];
    let real = real_path(path);
    if real != lexical {
        let target = name(&real, [real_path(cwd), real_path(root)]);
        if !out.contains(&target) {
            out.push(target);
        }
    }
    out
}

/// Expand the mentions of text the user typed. See `expand_from`.
pub fn expand(text: &str, ctx: &Ctx) -> (Value, Vec<String>) {
    expand_from(text, ctx, &Origin::User)
}

/// Expand the mentions of `text`, written by `origin`. Returns the JSON stored with the message (`Null` when there
/// is nothing) and one human-readable note per mention for the UI.
pub fn expand_from(text: &str, ctx: &Ctx, origin: &Origin) -> (Value, Vec<String>) {
    let (mut files, mut skill_items, mut agents, mut notes) = (vec![], vec![], vec![], vec![]);
    let decide = |action: &str, resources: &[String]| ctx.decide.map(|d| d(action, resources)).unwrap_or(Effect::Allow);
    let root = crate::tools::project_root_of(ctx.cwd);
    for tok in tokens(text) {
        let typed = origin.typed(&tok);
        let path = resolve_path(&tok, ctx.cwd);
        if path.exists() {
            let name = shown(&path, ctx.cwd);
            let effect = decide("read", &read_resources(&path, ctx.cwd, &root));
            if typed {
                // The user named it, which answers an `ask` (such as the one for `.env`); a `deny` still holds.
                if effect == Effect::Deny {
                    notes.push(format!("skipped {name}: your permission rules deny reading it"));
                    continue;
                }
            } else {
                let real = crate::tools::real_path(&path);
                let inside = [ctx.cwd, root.as_path()].iter().any(|b| real.starts_with(crate::tools::real_path(b)));
                let why = match effect {
                    _ if !inside => Some(format!("{} may only attach files inside the project", origin.label())),
                    Effect::Allow => None,
                    Effect::Ask => Some("reading it needs your permission; the model can ask for it with the read tool".to_string()),
                    Effect::Deny => Some("your permission rules deny reading it".to_string()),
                };
                if let Some(why) = why {
                    notes.push(format!("left @{tok} as text: {why}"));
                    continue;
                }
            }
            if path.is_dir() {
                let mut entries: Vec<String> = std::fs::read_dir(&path)
                    .map(|rd| rd.filter_map(|e| e.ok()).map(|e| format!("{}{}", e.file_name().to_string_lossy(), if e.path().is_dir() { "/" } else { "" })).collect())
                    .unwrap_or_default();
                entries.sort();
                let more = entries.len().saturating_sub(MAX_DIR_ENTRIES);
                entries.truncate(MAX_DIR_ENTRIES);
                if more > 0 {
                    entries.push(format!("... and {more} more"));
                }
                notes.push(format!("attached directory {name} ({} entries)", entries.len()));
                files.push(json!({"path": name, "kind": "dir", "content": entries.join("\n"), "truncated": more > 0}));
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            if meta.len() > media::MAX_PDF_BYTES as u64 {
                notes.push(format!("skipped {name}: larger than {} MiB", media::MAX_PDF_BYTES / 1024 / 1024));
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                notes.push(format!("skipped {name}: cannot be read"));
                continue;
            };
            if let Some(s) = media::sniff(&bytes) {
                // Images and PDFs, judged by their bytes.
                let accepted = if s.kind == media::Kind::Image { ctx.caps.image } else { ctx.caps.pdf };
                if !accepted {
                    notes.push(format!("skipped {name}: the current model does not accept {} input (see /model)", s.kind.label()));
                    continue;
                }
                if let Err(e) = media::check(&s, bytes.len()) {
                    notes.push(format!("skipped {name}: {e}"));
                    continue;
                }
                let Ok(hash) = blobs::put(ctx.app_dir, &bytes) else {
                    notes.push(format!("skipped {name}: could not store the attachment"));
                    continue;
                };
                notes.push(format!("attached {} {name} ({})", s.kind.label(), media::describe(&s, bytes.len())));
                files.push(json!({"path": name, "kind": "media", "mediaType": s.media_type, "blob": hash, "size": bytes.len(), "name": path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| name.clone())}));
                continue;
            }
            if meta.len() > MAX_FILE_BYTES {
                notes.push(format!("skipped {name}: larger than {} MiB (ask the model to read it in parts)", MAX_FILE_BYTES / 1024 / 1024));
                continue;
            }
            if bytes[..bytes.len().min(8192)].contains(&0) || std::str::from_utf8(&bytes).is_err() {
                notes.push(format!("skipped {name}: not a text file"));
                continue;
            }
            let (kept, truncated, total, n) = limit_text(&String::from_utf8_lossy(&bytes));
            notes.push(format!("attached {name} ({total} lines{})", if truncated { format!(", first {n} shown") } else { String::new() }));
            files.push(json!({"path": name, "kind": "file", "content": kept, "truncated": truncated, "lines": total}));
        } else if let Some(s) = ctx.skills.iter().find(|s| s.id == tok) {
            notes.push(format!("loaded skill {}", s.id));
            skill_items.push(json!({"id": s.id, "content": skills::render_content(s)}));
        } else if ctx.subagents.contains(&tok) {
            notes.push(format!("@{tok}: the model will be asked to delegate to it"));
            agents.push(json!(tok));
        } else if let Some(r) = ctx.mcp.and_then(|m| m.find_resource(&tok).map(|r| (m, r))) {
            let (m, r) = r;
            let effect = decide("mcp-resource", &[format!("{}:{}", r.server, r.uri)]);
            let own = matches!(origin, Origin::McpPrompt { server } if *server == r.server);
            if effect == Effect::Deny || !(typed || own || effect == Effect::Allow) {
                let why = if effect == Effect::Deny { "your permission rules deny reading it" } else { "reading it needs your permission; the model can ask for it with the mcp-resource tool" };
                notes.push(if typed { format!("skipped MCP resource {}:{}: {why}", r.server, r.name) } else { format!("left @{tok} as text: {why}") });
                continue;
            }
            let label = format!("mcp://{}/{}", r.server, r.name);
            if files.iter().any(|f| f["path"] == label.as_str()) {
                continue; // the same resource named twice (by name and by URI) is attached once
            }
            match m.read_resource(&r.server, &r.uri) {
                Ok(text) => {
                    let (kept, truncated, total, n) = limit_text(&text);
                    notes.push(format!("attached MCP resource {}:{} ({total} lines{})", r.server, r.name, if truncated { format!(", first {n} shown") } else { String::new() }));
                    files.push(json!({"path": label, "kind": "file", "content": kept, "truncated": truncated, "lines": total}));
                }
                Err(e) => notes.push(format!("skipped MCP resource {}:{}: {e}", r.server, r.name)),
            }
        }
    }
    if files.is_empty() && skill_items.is_empty() && agents.is_empty() {
        return (Value::Null, notes);
    }
    (json!({"files": files, "skills": skill_items, "agents": agents}), notes)
}

/// Content blocks for a stored `mentions` value, appended after the typed text when the message goes to the model.
/// Attachments are loaded through `load`.
pub fn render_blocks(mentions: &Value, load: crate::chat::Loader) -> Vec<Block> {
    let mut out = vec![];
    for f in mentions["files"].as_array().into_iter().flatten() {
        let path = f["path"].as_str().unwrap_or("");
        if f["kind"] == "media" {
            out.push(Block::Text(format!("<attachment path=\"{path}\" />")));
            out.push(crate::chat::media_block(f, load));
            continue;
        }
        let content = f["content"].as_str().unwrap_or("");
        let content = content.strip_suffix('\n').unwrap_or(content); // the closing tag goes on its own line
        let tag = if f["kind"] == "dir" { "directory" } else { "file" };
        let note = if f["truncated"] == true { "\n[truncated: use the read tool for the rest]" } else { "" };
        out.push(Block::Text(format!("<{tag} path=\"{path}\">\n{content}{note}\n</{tag}>")));
    }
    for s in mentions["skills"].as_array().into_iter().flatten() {
        out.push(Block::Text(s["content"].as_str().unwrap_or("").to_string()));
    }
    for a in mentions["agents"].as_array().into_iter().flatten() {
        let a = a.as_str().unwrap_or("");
        out.push(Block::Text(format!("<system-reminder>\nThe user mentioned the \"{a}\" agent with @{a}. Delegate the relevant part of the request to it with the subagent tool (agent: \"{a}\").\n</system-reminder>")));
    }
    out
}

/// Text-only form (for prompts that become a subagent's task): attachments that are images or PDFs are not forwarded.
pub fn render(mentions: &Value) -> Vec<String> {
    render_blocks(mentions, &|_| None)
        .into_iter()
        .filter_map(|b| match b {
            Block::Text(t) if t.starts_with("[attachment ") && t.ends_with("is no longer available]") => None,
            Block::Text(t) => Some(t),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::tests::png;

    #[test]
    fn tokens_start_at_word_boundaries_and_drop_punctuation() {
        assert_eq!(tokens("look at @src/main.rs, then @README.md. Also (@docs/) and \"@a b\""), ["src/main.rs", "README.md", "docs/", "a"]);
        assert_eq!(tokens("mail me@example.com or @@ or @ alone @"), Vec::<String>::new());
        assert_eq!(tokens("@x @x @y"), ["x", "y"], "deduplicated");
        assert_eq!(tokens("@\u{e9}t\u{e9}.txt!"), ["\u{e9}t\u{e9}.txt"], "non-ASCII is fine");
        assert_eq!(tokens("@~/notes.md"), ["~/notes.md"]);
        assert_eq!(tokens("see @/Users/me/Screen\\ Shot\\ 2024.png now"), ["/Users/me/Screen Shot 2024.png"], "dragged-in paths keep their escaped spaces");
        assert_eq!(tokens("@a\\ b.txt, @c"), ["a b.txt", "c"]);
    }

    struct Fx {
        root: PathBuf,
        skills: Vec<Skill>,
    }
    impl Fx {
        fn work(&self) -> PathBuf {
            self.root.join("work")
        }
        fn ctx<'a>(&'a self, work: &'a Path, subagents: &'a [String], caps: MediaCaps) -> Ctx<'a> {
            Ctx { cwd: work, skills: &self.skills, subagents, app_dir: &self.root, caps, mcp: None, decide: None }
        }
    }
    impl Drop for Fx {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn setup() -> Fx {
        let root = std::env::temp_dir().join(crate::id::new_id("moco-mention"));
        std::fs::create_dir_all(root.join("work/sub")).unwrap();
        std::fs::write(root.join("work/a.txt"), "one\ntwo\r\nthree\n").unwrap();
        std::fs::write(root.join("work/sub/b.rs"), "fn b() {}").unwrap();
        std::fs::write(root.join("work/bin.dat"), [0u8, 1, 2]).unwrap();
        std::fs::write(root.join("outside.txt"), "outside").unwrap();
        std::fs::create_dir_all(root.join("skill/pdf")).unwrap();
        std::fs::write(root.join("skill/pdf/SKILL.md"), "---\ndescription: pdf things\n---\nPDF steps").unwrap();
        let skills = skills::discover(&root.join("app"), &root.join("work"), &[root.join("skill").to_string_lossy().into_owned()]);
        Fx { root, skills }
    }

    #[test]
    fn files_dirs_skills_agents_and_nonmentions() {
        let fx = setup();
        let work = fx.work();
        let subagents = vec!["explore".to_string()];
        let ctx = fx.ctx(&work, &subagents, MediaCaps::default());
        let (m, notes) = expand("read @a.txt and @sub and @../outside.txt, use @pdf, ask @explore, ignore @nothing @bin.dat", &ctx);
        let files = m["files"].as_array().unwrap();
        assert_eq!(files.iter().map(|f| f["path"].as_str().unwrap()).collect::<Vec<_>>(), ["a.txt", "sub", &fx.root.join("outside.txt").display().to_string()]);
        assert_eq!(files[0]["content"], "one\ntwo\nthree\n", "CRLF normalized");
        assert_eq!((files[1]["kind"].as_str(), files[1]["content"].as_str()), (Some("dir"), Some("b.rs")));
        assert_eq!(m["skills"][0]["id"], "pdf");
        assert!(m["skills"][0]["content"].as_str().unwrap().contains("PDF steps"));
        assert_eq!(m["agents"], json!(["explore"]));
        assert!(notes.iter().any(|n| n == "attached a.txt (3 lines)"), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("skipped bin.dat: not a text file")), "{notes:?}");
        assert!(!notes.iter().any(|n| n.contains("nothing")), "unknown mentions are left as plain text");
        assert_eq!(expand("just text, no mentions", &ctx).0, Value::Null);
        // A file wins over a skill or agent with the same name.
        std::fs::write(work.join("pdf"), "a file called pdf").unwrap();
        let (m, _) = expand("@pdf", &ctx);
        assert_eq!((m["files"][0]["content"].as_str(), m["skills"].as_array().unwrap().len()), (Some("a file called pdf"), 0));
    }

    fn attached(m: &Value) -> Vec<String> {
        m["files"].as_array().into_iter().flatten().map(|f| f["path"].as_str().unwrap().to_string()).collect()
    }

    /// Rules of auto mode (everything allowed, `.env` asks) with a deny on `secret.txt`, decided the way
    /// `Engine::decide_now` does.
    fn auto_rules() -> impl Fn(&str, &[String]) -> Effect {
        let mut rules = crate::permission::default_rules();
        rules.push(crate::permission::Rule::new("read", "*secret.txt", Effect::Deny));
        move |action: &str, resources: &[String]| crate::permission::decide(action, resources, &rules, &[])
    }

    #[test]
    fn a_project_command_attaches_only_project_files_that_read_allows_without_asking() {
        let fx = setup();
        let work = fx.work();
        std::fs::write(work.join(".env"), "TOKEN=hunter2").unwrap();
        std::fs::write(work.join("secret.txt"), "denied").unwrap();
        std::os::unix::fs::symlink(fx.root.join("outside.txt"), work.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(work.join(".env"), work.join("notes.txt")).unwrap();
        let rules = auto_rules();
        let ctx = Ctx { decide: Some(&rules), ..fx.ctx(&work, &[], MediaCaps::default()) };
        let outside = fx.root.join("outside.txt").display().to_string();
        let template = format!("Review @a.txt @sub. Also @../outside.txt @{outside} @/etc/hosts @~/ @.env @notes.txt @link.txt @secret.txt");
        let origin = Origin::Command { args: String::new() };
        let (m, notes) = expand_from(&template, &ctx, &origin);
        assert_eq!(attached(&m), ["a.txt", "sub"], "{notes:?}");
        let left = |tok: &str| notes.iter().any(|n| n.starts_with(&format!("left @{tok} as text: ")));
        for tok in ["../outside.txt", outside.as_str(), "link.txt", ".env", "notes.txt", "secret.txt"] {
            assert!(left(tok), "@{tok} is reported as left alone: {notes:?}");
        }
        if Path::new("/etc/hosts").exists() {
            assert!(left("/etc/hosts"), "{notes:?}");
        }
        if !work.starts_with(std::env::var("HOME").unwrap_or_default()) {
            assert!(left("~/"), "the home directory is not listed: {notes:?}");
        }
        assert!(notes.iter().any(|n| n == "left @../outside.txt as text: a project command may only attach files inside the project"), "{notes:?}");
        assert!(notes.iter().any(|n| n.starts_with("left @.env as text: reading it needs your permission")), "{notes:?}");
        assert!(notes.iter().any(|n| n == "left @secret.txt as text: your permission rules deny reading it"), "{notes:?}");
        assert!(!m.to_string().contains("hunter2") && !m.to_string().contains("\"outside\""));

        // What the user typed after the command name is theirs: `@$ARGUMENTS` with `../outside.txt` attaches it.
        let (m, _) = expand_from("Review @../outside.txt and @.env", &ctx, &Origin::Command { args: "../outside.txt".into() });
        assert_eq!(attached(&m), [outside.clone()], "typed by the user, unlike the template's @.env");
        let (m, _) = expand_from("Compare @../outside.txt", &ctx, &Origin::Command { args: "see @../outside.txt".into() });
        assert_eq!(attached(&m), [outside.clone()]);

        // Manual mode allows reads but asks for `.env`, and the project check does not depend on the mode.
        let manual = crate::permission::mode_rules(crate::permission::PermissionMode::Manual);
        let decide = move |a: &str, r: &[String]| crate::permission::decide(a, r, &manual, &[]);
        let ctx = Ctx { decide: Some(&decide), ..fx.ctx(&work, &[], MediaCaps::default()) };
        let (m, notes) = expand_from(&template, &ctx, &origin);
        assert_eq!(attached(&m), ["a.txt", "sub", "secret.txt"], "no deny rule here: {notes:?}");
        // "Inside" means the project root (`tools::project_root_of`), not only the working directory.
        std::fs::create_dir_all(fx.root.join(".git")).unwrap();
        assert_eq!(attached(&expand_from("@../outside.txt @link.txt", &ctx, &origin).0), [outside.clone(), "link.txt".into()]);
    }

    #[test]
    fn an_mcp_prompt_gets_the_same_checks_and_no_typed_words() {
        let fx = setup();
        let work = fx.work();
        std::fs::write(work.join(".env"), "TOKEN=hunter2").unwrap();
        let rules = auto_rules();
        let ctx = Ctx { decide: Some(&rules), ..fx.ctx(&work, &[], MediaCaps::default()) };
        let origin = Origin::McpPrompt { server: "srv".into() };
        let (m, notes) = expand_from("@a.txt @../outside.txt @.env @pdf", &ctx, &origin);
        assert_eq!(attached(&m), ["a.txt"]);
        assert!(notes.iter().any(|n| n == "left @../outside.txt as text: a prompt of MCP server 'srv' may only attach files inside the project"), "{notes:?}");
        assert!(notes.iter().any(|n| n.starts_with("left @.env as text")), "{notes:?}");
        assert_eq!(m["skills"][0]["id"], "pdf", "skills still load (the skill tool needs no permission either)");
    }

    #[test]
    fn typed_mentions_attach_anything_except_what_a_rule_denies() {
        let fx = setup();
        let work = fx.work();
        std::fs::write(work.join(".env"), "TOKEN=hunter2").unwrap();
        std::fs::write(work.join("secret.txt"), "denied").unwrap();
        std::os::unix::fs::symlink(work.join("secret.txt"), work.join("alias.txt")).unwrap();
        let rules = auto_rules();
        let ctx = Ctx { decide: Some(&rules), ..fx.ctx(&work, &[], MediaCaps::default()) };
        let (m, notes) = expand("@.env @../outside.txt @secret.txt @alias.txt", &ctx);
        assert_eq!(attached(&m), [".env".to_string(), fx.root.join("outside.txt").display().to_string()], "typing it answers the .env ask: {notes:?}");
        assert!(notes.iter().any(|n| n == "skipped secret.txt: your permission rules deny reading it"), "{notes:?}");
        assert!(notes.iter().any(|n| n == "skipped alias.txt: your permission rules deny reading it"), "a link to a denied file is denied: {notes:?}");
    }

    #[test]
    fn images_and_pdfs_become_blobs_judged_by_bytes_and_by_what_the_model_accepts() {
        let fx = setup();
        let work = fx.work();
        std::fs::write(work.join("Screen Shot.png"), png(640, 480)).unwrap();
        std::fs::write(work.join("spec"), b"%PDF-1.7 body").unwrap(); // no extension
        std::fs::write(work.join("huge.png"), png(9000, 10)).unwrap();
        std::fs::write(work.join("fake.png"), "plain text pretending").unwrap();
        let ctx = fx.ctx(&work, &[], MediaCaps::default());
        let (m, notes) = expand("@Screen\\ Shot.png @spec @huge.png @fake.png", &ctx);
        let files = m["files"].as_array().unwrap();
        assert_eq!(files.len(), 3, "{notes:?}");
        assert_eq!((files[0]["kind"].as_str(), files[0]["mediaType"].as_str(), files[0]["name"].as_str()), (Some("media"), Some("image/png"), Some("Screen Shot.png")));
        assert_eq!(files[1]["mediaType"], "application/pdf");
        assert_eq!(files[2]["kind"], "file", "a fake .png is just a text file");
        assert!(notes.iter().any(|n| n == "attached image Screen Shot.png (image/png, 640x480, 1 KiB)") && notes.iter().any(|n| n.contains("skipped huge.png: the image is 9000x10")), "{notes:?}");
        // The bytes are in the blob store, not in the message.
        let hash = files[0]["blob"].as_str().unwrap();
        assert_eq!(blobs::get(&fx.root, hash).unwrap(), png(640, 480));
        assert!(!m.to_string().contains("iVBOR"), "no base64 in the stored message");
        // Rendering loads them back.
        let blocks = render_blocks(&m, &|h| blobs::get(&fx.root, h));
        assert!(matches!(&blocks[0], Block::Text(t) if t == "<attachment path=\"Screen Shot.png\" />"));
        assert!(matches!(&blocks[1], Block::Image { media_type, .. } if media_type == "image/png"));
        assert!(matches!(&blocks[3], Block::Document { name, .. } if name == "spec"));
        // A model that cannot see images gets a note instead of an attachment.
        let (m, notes) = expand("@Screen\\ Shot.png @spec", &fx.ctx(&work, &[], MediaCaps { image: false, pdf: true }));
        assert_eq!(m["files"].as_array().unwrap().len(), 1);
        assert!(notes.iter().any(|n| n.contains("does not accept image input")), "{notes:?}");
        // Text-only rendering (for subagent prompts) leaves media out.
        assert!(render(&expand("@Screen\\ Shot.png @a.txt", &ctx).0).iter().all(|t| !t.contains("Screen Shot.png\" />") || t.starts_with("<attachment")));
    }

    #[test]
    fn big_and_long_files_are_limited() {
        let fx = setup();
        let work = fx.work();
        std::fs::write(work.join("long.txt"), (1..=3000).map(|i| i.to_string()).collect::<Vec<_>>().join("\n")).unwrap();
        std::fs::write(work.join("huge.txt"), "x".repeat(3 * 1024 * 1024)).unwrap();
        std::fs::write(work.join("giant.bin"), vec![b'x'; 11 * 1024 * 1024]).unwrap();
        let ctx = fx.ctx(&work, &[], MediaCaps::default());
        let (m, notes) = expand("@long.txt @huge.txt @giant.bin", &ctx);
        let f = &m["files"][0];
        assert_eq!((f["truncated"].as_bool(), f["lines"].as_u64()), (Some(true), Some(3000)));
        assert!(f["content"].as_str().unwrap().ends_with("2000") && !f["content"].as_str().unwrap().contains("2001"));
        assert!(notes.iter().any(|n| n.contains("first 2000 shown")) && notes.iter().any(|n| n.contains("skipped huge.txt: larger than 2 MiB")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("skipped giant.bin: larger than 10 MiB")), "{notes:?}");
        assert_eq!(m["files"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn mcp_resources_can_be_mentioned() {
        if std::process::Command::new("python3").arg("--version").output().map(|o| !o.status.success()).unwrap_or(true) {
            return eprintln!("skipped: python3 not available");
        }
        let fx = setup();
        let work = fx.work();
        let spec = crate::mcp::ServerSpec {
            name: "fake".into(),
            config: crate::config::McpServerConfig { command: vec!["python3".into(), concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_mcp.py").into()], ..Default::default() },
            cwd: work.clone(),
        };
        let m = crate::mcp::Manager::start(vec![spec], &Default::default(), &std::env::temp_dir());
        assert!(m.wait_ready(std::time::Duration::from_secs(20)));
        let ctx = Ctx { mcp: Some(&m), ..fx.ctx(&work, &[], MediaCaps::default()) };
        let (v, notes) = expand("what is in @fake:notes and again @file:///notes.txt, ignoring @fake:missing", &ctx);
        let files = v["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "one resource, named twice, is attached once: {files:?}");
        assert_eq!((files[0]["path"].as_str(), files[0]["content"].as_str()), (Some("mcp://fake/notes"), Some("remember the milk")));
        assert_eq!(notes, ["attached MCP resource fake:notes (1 lines)"], "unknown names are left as plain text");
        let blocks = render_blocks(&v, &|_| None);
        assert!(matches!(&blocks[0], Block::Text(t) if t == "<file path=\"mcp://fake/notes\">\nremember the milk\n</file>"));

        // When `mcp-resource` asks: the user's mention and the server's own prompt attach it, nothing else does.
        let ask = |a: &str, _: &[String]| if a == "mcp-resource" { Effect::Ask } else { Effect::Allow };
        let ctx = Ctx { mcp: Some(&m), decide: Some(&ask), ..fx.ctx(&work, &[], MediaCaps::default()) };
        assert_eq!(attached(&expand("@fake:notes", &ctx).0), ["mcp://fake/notes"]);
        assert_eq!(attached(&expand_from("@fake:notes", &ctx, &Origin::McpPrompt { server: "fake".into() }).0), ["mcp://fake/notes"]);
        for origin in [Origin::McpPrompt { server: "other".into() }, Origin::Command { args: String::new() }] {
            let (v, notes) = expand_from("@fake:notes", &ctx, &origin);
            assert_eq!(v, Value::Null, "{origin:?}");
            assert_eq!(notes, ["left @fake:notes as text: reading it needs your permission; the model can ask for it with the mcp-resource tool"]);
        }
        let deny = |a: &str, _: &[String]| if a == "mcp-resource" { Effect::Deny } else { Effect::Allow };
        let ctx = Ctx { mcp: Some(&m), decide: Some(&deny), ..fx.ctx(&work, &[], MediaCaps::default()) };
        assert_eq!(expand("@fake:notes", &ctx).1, ["skipped MCP resource fake:notes: your permission rules deny reading it"]);
        assert_eq!(expand_from("@fake:notes", &ctx, &Origin::McpPrompt { server: "fake".into() }).0, Value::Null, "a deny holds for the server's own prompt too");
        m.shutdown();
    }

    #[test]
    fn rendering_for_the_model() {
        let m = json!({"files":[{"path":"a.txt","kind":"file","content":"hi","truncated":false},{"path":"d","kind":"dir","content":"x\ny/","truncated":false},{"path":"big","kind":"file","content":"cut","truncated":true}],
                       "skills":[{"id":"s","content":"<skill_content>S</skill_content>"}],"agents":["explore"]});
        let r = render(&m);
        assert_eq!(r[0], "<file path=\"a.txt\">\nhi\n</file>");
        assert_eq!(r[1], "<directory path=\"d\">\nx\ny/\n</directory>");
        assert!(r[2].contains("[truncated: use the read tool for the rest]"));
        assert_eq!(r[3], "<skill_content>S</skill_content>");
        assert!(r[4].contains("subagent tool (agent: \"explore\")") && r[4].starts_with("<system-reminder>"));
        assert!(render(&Value::Null).is_empty());
    }
}
