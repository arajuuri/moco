//! Candidates for the composer's `/` and `@` completion.
use super::app::Item;
use moco_core::mcp::Manager;
use std::sync::{Arc, Mutex};

const LIMIT: usize = 8;
const MAX_FILES: usize = 30_000;

pub struct Completer {
    files: Arc<Mutex<Vec<String>>>,
    skills: Vec<(String, String)>,
    agents: Vec<(String, String)>,
    commands: Vec<(String, String)>,
    mcp: Option<Arc<Manager>>,
}

impl Completer {
    /// `skills`, `agents` and `commands` are (name, description). The project's files are listed on a
    /// background thread (respecting .gitignore), so completion works immediately and gets better.
    pub fn new(cwd: &std::path::Path, skills: Vec<(String, String)>, agents: Vec<(String, String)>, commands: Vec<(String, String)>, mcp: Option<Arc<Manager>>) -> Completer {
        let files = Arc::new(Mutex::new(vec![]));
        let (out, root) = (files.clone(), cwd.to_path_buf());
        std::thread::spawn(move || {
            let walk = ignore::WalkBuilder::new(&root).max_depth(Some(10)).build();
            for entry in walk.flatten() {
                let Ok(rel) = entry.path().strip_prefix(&root) else { continue };
                let mut s = rel.to_string_lossy().replace('\\', "/");
                if s.is_empty() {
                    continue;
                }
                if entry.file_type().is_some_and(|t| t.is_dir()) {
                    s.push('/');
                }
                let mut v = out.lock().unwrap();
                if v.len() >= MAX_FILES {
                    break;
                }
                v.push(s);
            }
        });
        Completer { files, skills, agents, commands, mcp }
    }

    #[cfg(test)]
    pub fn with_files(files: Vec<&str>, skills: Vec<(String, String)>, agents: Vec<(String, String)>, commands: Vec<(String, String)>) -> Completer {
        Completer { files: Arc::new(Mutex::new(files.into_iter().map(String::from).collect())), skills, agents, commands, mcp: None }
    }

    /// Candidates for a token that starts with `/` or `@`.
    pub fn items(&self, token: &str) -> Vec<Item> {
        if let Some(q) = token.strip_prefix('/') {
            return self.slash(q);
        }
        match token.strip_prefix('@') {
            Some(q) => self.mention(&q.replace("\\ ", " ")),
            None => vec![],
        }
    }

    fn slash(&self, q: &str) -> Vec<Item> {
        let mut all: Vec<(String, String)> = vec![("help".into(), "show the keys".into()), ("exit".into(), "quit".into()), ("resume".into(), "continue a session by id".into())];
        all.extend(super::builtin::NAMES.iter().map(|(n, d)| (n.to_string(), d.to_string())));
        all.extend(self.commands.iter().cloned());
        if let Some(m) = &self.mcp {
            all.extend(m.prompts().into_iter().map(|p| (p.command, format!("{} [mcp]", p.description))));
        }
        rank(all.into_iter().map(|(n, d)| (n.clone(), Item { insert: format!("/{n}"), label: format!("/{n}"), detail: d })).collect(), q)
    }

    fn mention(&self, q: &str) -> Vec<Item> {
        let mut named: Vec<(String, Item)> = vec![];
        for (n, d) in &self.agents {
            named.push((n.clone(), Item { insert: format!("@{n}"), label: format!("@{n}"), detail: format!("agent: {d}") }));
        }
        for (n, d) in &self.skills {
            named.push((n.clone(), Item { insert: format!("@{n}"), label: format!("@{n}"), detail: format!("skill: {d}") }));
        }
        if let Some(m) = &self.mcp {
            for r in m.resources() {
                let n = format!("{}:{}", r.server, r.name);
                named.push((n.clone(), Item { insert: format!("@{n}"), label: format!("@{n}"), detail: format!("mcp: {}", r.description) }));
            }
        }
        let mut out = rank(named, q);
        let files = self.files.lock().unwrap();
        out.extend(rank_paths(&files, q, LIMIT.saturating_sub(out.len()).max(4)));
        out.truncate(LIMIT);
        out
    }
}

/// Prefix matches first, then substring matches; alphabetical within each.
fn rank(items: Vec<(String, Item)>, q: &str) -> Vec<Item> {
    let q = q.to_lowercase();
    let mut scored: Vec<(u8, String, Item)> = items
        .into_iter()
        .filter_map(|(name, it)| {
            let l = name.to_lowercase();
            if l.starts_with(&q) {
                Some((0, l, it))
            } else if l.contains(&q) {
                Some((1, l, it))
            } else {
                None
            }
        })
        .collect();
    scored.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    scored.into_iter().map(|(_, _, it)| it).take(LIMIT).collect()
}

fn escape(path: &str) -> String {
    path.replace(' ', "\\ ")
}

/// Best matching paths: by base name prefix, base name substring, path substring, then subsequence.
fn rank_paths(paths: &[String], q: &str, limit: usize) -> Vec<Item> {
    let q = q.to_lowercase();
    let mut scored: Vec<(u32, usize, &String)> = vec![];
    for p in paths {
        let l = p.to_lowercase();
        let trimmed = l.trim_end_matches('/');
        let base = trimmed.rsplit('/').next().unwrap_or(trimmed);
        let depth = trimmed.matches('/').count() as u32;
        let score = if q.is_empty() {
            if depth > 0 { continue } else { 0 }
        } else if base.starts_with(&q) {
            depth
        } else if base.contains(&q) {
            100 + depth
        } else if let Some(pos) = l.find(&q) {
            200 + pos as u32
        } else if subsequence(&l, &q) {
            400 + l.len() as u32
        } else {
            continue;
        };
        scored.push((score, p.len(), p));
    }
    scored.sort();
    scored
        .into_iter()
        .take(limit)
        .map(|(_, _, p)| {
            let dir = p.ends_with('/');
            Item { insert: format!("@{}", escape(p)), label: format!("@{p}"), detail: if dir { "directory".into() } else { String::new() } }
        })
        .collect()
}

fn subsequence(hay: &str, needle: &str) -> bool {
    let mut it = hay.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c() -> Completer {
        Completer::with_files(
            vec!["README.md", "src/", "src/main.rs", "src/tui/", "src/tui/app.rs", "docs/", "docs/12-tui-plan.md", "my file.txt", "Cargo.toml"],
            vec![("deploy".into(), "ship it".into())],
            vec![("explore".into(), "search the code".into())],
            vec![("review".into(), "review changes".into()), ("init".into(), "create AGENTS.md".into())],
        )
    }

    fn labels(v: &[Item]) -> Vec<&str> {
        v.iter().map(|i| i.label.as_str()).collect()
    }

    #[test]
    fn slash_commands_match_by_prefix_before_substring() {
        let c = c();
        assert_eq!(labels(&c.items("/"))[..3], ["/agent", "/archive", "/commands"]);
        assert_eq!(labels(&c.items("/i"))[0], "/init"); // prefix before substring
        assert_eq!(labels(&c.items("/re"))[..4], ["/redo", "/rename", "/resume", "/review"]);
        assert!(labels(&c.items("/it")).starts_with(&["/exit", "/init"])); // substring
        assert!(c.items("/zzz").is_empty());
    }

    #[test]
    fn mentions_offer_agents_skills_then_files() {
        let c = c();
        assert_eq!(labels(&c.items("@")), vec!["@deploy", "@explore", "@src/", "@docs/", "@README.md", "@Cargo.toml", "@my file.txt"]);
        let r = c.items("@app");
        assert_eq!(r[0].label, "@src/tui/app.rs");
        let r = c.items("@tui");
        assert_eq!(labels(&r), ["@src/tui/", "@docs/12-tui-plan.md", "@src/tui/app.rs"]); // base-name hits before path hits
        assert_eq!(c.items("@my")[0].insert, "@my\\ file.txt"); // spaces are escaped the way mentions parse them
        assert_eq!(c.items("@my\\ f")[0].label, "@my file.txt");
        assert_eq!(labels(&c.items("@dpl")), vec!["@docs/12-tui-plan.md"]); // names need a substring, paths accept a subsequence
        assert!(c.items("@zzz").is_empty());
    }
}
