//! `@` mentions in a prompt (docs/09 "Composer"): `@path` attaches a file or directory listing, `@skill` loads a
//! skill, `@agent` asks the model to delegate to that subagent. Everything is resolved when the prompt is
//! submitted and stored with the message, so a conversation replays exactly as it was sent.
//! An `@` only starts a mention at the beginning of a word, so `me@example.com` is left alone.
use crate::skills::{self, Skill};
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
}

/// Distinct `@tokens` of a prompt, trailing punctuation removed, in order of appearance.
pub fn tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (pos, c) = chars[i];
        let starts_word = pos == 0 || chars[i - 1].1.is_whitespace() || matches!(chars[i - 1].1, '(' | '[' | '{' | ',' | '"' | '\'');
        if c == '@' && starts_word {
            let mut j = i + 1;
            while j < chars.len() && !chars[j].1.is_whitespace() {
                j += 1;
            }
            let end = chars.get(j).map(|(p, _)| *p).unwrap_or(text.len());
            let tok = text[pos + 1..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"']);
            if !tok.is_empty() && !tok.starts_with('@') && !out.iter().any(|t| t == tok) {
                out.push(tok.to_string());
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

/// Expand the mentions of `text`. Returns the JSON stored with the message (`Null` when there is nothing) and
/// one human-readable note per mention for the UI.
pub fn expand(text: &str, ctx: &Ctx) -> (Value, Vec<String>) {
    let (mut files, mut skill_items, mut agents, mut notes) = (vec![], vec![], vec![], vec![]);
    for tok in tokens(text) {
        let path = resolve_path(&tok, ctx.cwd);
        if path.exists() {
            let name = shown(&path, ctx.cwd);
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
            if meta.len() > MAX_FILE_BYTES {
                notes.push(format!("skipped {name}: larger than {} MiB (ask the model to read it in parts)", MAX_FILE_BYTES / 1024 / 1024));
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                notes.push(format!("skipped {name}: cannot be read"));
                continue;
            };
            if bytes[..bytes.len().min(8192)].contains(&0) || std::str::from_utf8(&bytes).is_err() {
                notes.push(format!("skipped {name}: not a text file"));
                continue;
            }
            let full = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
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
            let truncated = n < pieces;
            notes.push(format!("attached {name} ({total} lines{})", if truncated { format!(", first {n} shown") } else { String::new() }));
            files.push(json!({"path": name, "kind": "file", "content": kept, "truncated": truncated, "lines": total}));
        } else if let Some(s) = ctx.skills.iter().find(|s| s.id == tok) {
            notes.push(format!("loaded skill {}", s.id));
            skill_items.push(json!({"id": s.id, "content": skills::render_content(s)}));
        } else if ctx.subagents.iter().any(|a| *a == tok) {
            notes.push(format!("@{tok}: the model will be asked to delegate to it"));
            agents.push(json!(tok));
        }
    }
    if files.is_empty() && skill_items.is_empty() && agents.is_empty() {
        return (Value::Null, notes);
    }
    (json!({"files": files, "skills": skill_items, "agents": agents}), notes)
}

/// Text blocks for a stored `mentions` value, appended after the typed text when the message is sent to the model.
pub fn render(mentions: &Value) -> Vec<String> {
    let mut out = vec![];
    for f in mentions["files"].as_array().into_iter().flatten() {
        let (path, content) = (f["path"].as_str().unwrap_or(""), f["content"].as_str().unwrap_or(""));
        let content = content.strip_suffix('\n').unwrap_or(content); // the closing tag goes on its own line
        let tag = if f["kind"] == "dir" { "directory" } else { "file" };
        let note = if f["truncated"] == true { "\n[truncated: use the read tool for the rest]" } else { "" };
        out.push(format!("<{tag} path=\"{path}\">\n{content}{note}\n</{tag}>"));
    }
    for s in mentions["skills"].as_array().into_iter().flatten() {
        out.push(s["content"].as_str().unwrap_or("").to_string());
    }
    for a in mentions["agents"].as_array().into_iter().flatten() {
        let a = a.as_str().unwrap_or("");
        out.push(format!("<system-reminder>\nThe user mentioned the \"{a}\" agent with @{a}. Delegate the relevant part of the request to it with the subagent tool (agent: \"{a}\").\n</system-reminder>"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_start_at_word_boundaries_and_drop_punctuation() {
        assert_eq!(tokens("look at @src/main.rs, then @README.md. Also (@docs/) and \"@a b\""), ["src/main.rs", "README.md", "docs/", "a"]);
        assert_eq!(tokens("mail me@example.com or @@ or @ alone @"), Vec::<String>::new());
        assert_eq!(tokens("@x @x @y"), ["x", "y"], "deduplicated");
        assert_eq!(tokens("@\u{e9}t\u{e9}.txt!"), ["\u{e9}t\u{e9}.txt"], "non-ASCII is fine");
        assert_eq!(tokens("@~/notes.md"), ["~/notes.md"]);
    }

    fn setup() -> (PathBuf, Vec<Skill>) {
        let root = std::env::temp_dir().join(crate::id::new_id("moco-mention"));
        std::fs::create_dir_all(root.join("work/sub")).unwrap();
        std::fs::write(root.join("work/a.txt"), "one\ntwo\r\nthree\n").unwrap();
        std::fs::write(root.join("work/sub/b.rs"), "fn b() {}").unwrap();
        std::fs::write(root.join("work/bin.dat"), [0u8, 1, 2]).unwrap();
        std::fs::write(root.join("outside.txt"), "outside").unwrap();
        std::fs::create_dir_all(root.join("skill/pdf")).unwrap();
        std::fs::write(root.join("skill/pdf/SKILL.md"), "---\ndescription: pdf things\n---\nPDF steps").unwrap();
        let skills = skills::discover(&root.join("app"), &root.join("work"), &[root.join("skill").to_string_lossy().into_owned()]);
        (root, skills)
    }

    #[test]
    fn files_dirs_skills_agents_and_nonmentions() {
        let (root, skills) = setup();
        let work = root.join("work");
        let subagents = vec!["explore".to_string()];
        let ctx = Ctx { cwd: &work, skills: &skills, subagents: &subagents };
        let (m, notes) = expand("read @a.txt and @sub and @../outside.txt, use @pdf, ask @explore, ignore @nothing @bin.dat", &ctx);
        let files = m["files"].as_array().unwrap();
        assert_eq!(files.iter().map(|f| f["path"].as_str().unwrap()).collect::<Vec<_>>(), ["a.txt", "sub", &root.join("outside.txt").display().to_string()]);
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
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn big_and_long_files_are_limited() {
        let (root, skills) = setup();
        let work = root.join("work");
        std::fs::write(work.join("long.txt"), (1..=3000).map(|i| i.to_string()).collect::<Vec<_>>().join("\n")).unwrap();
        std::fs::write(work.join("huge.txt"), "x".repeat(3 * 1024 * 1024)).unwrap();
        let ctx = Ctx { cwd: &work, skills: &skills, subagents: &[] };
        let (m, notes) = expand("@long.txt @huge.txt", &ctx);
        let f = &m["files"][0];
        assert_eq!((f["truncated"].as_bool(), f["lines"].as_u64()), (Some(true), Some(3000)));
        assert!(f["content"].as_str().unwrap().ends_with("2000") && !f["content"].as_str().unwrap().contains("2001"));
        assert!(notes.iter().any(|n| n.contains("first 2000 shown")) && notes.iter().any(|n| n.contains("skipped huge.txt: larger than 2 MiB")), "{notes:?}");
        assert_eq!(m["files"].as_array().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(root);
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
