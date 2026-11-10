use crate::compat::*;
use super::*;
use ignore::WalkBuilder;
use regex::RegexBuilder;
use std::io::BufReader;

/// Matches are counted exactly up to `limit * COUNT_FACTOR`, then the search stops.
const COUNT_FACTOR: usize = 10;

pub struct Grep;

/// Files bigger than this are skipped (and reported).
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// Only this much of a line is searched, so one enormous line cannot be buffered whole.
const MAX_LINE_BYTES: usize = 64 * 1024;

impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }
    fn description(&self) -> String {
        "Search file contents with a regular expression (Rust regex syntax) or a literal string. Returns `path:line: text`. Respects .gitignore.".into()
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "pattern":{"type":"string"},"path":{"type":"string"},
            "include":{"type":"string","description":"Glob filter on file paths, e.g. \"*.rs\""},
            "literal":{"type":"boolean","description":"Treat pattern as a literal string (default false)"},
            "caseSensitive":{"type":"boolean","description":"Default true"},
            "limit":{"type":"integer","minimum":1,"description":"Max matches (default 100)"}},"required":["pattern"]})
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let pattern = str_arg(input, "pattern")?;
        let root = opt_str(input, "path").map(|p| ctx.resolve(p)).unwrap_or_else(|| ctx.cwd.clone());
        let limit = opt_u64(input, "limit").unwrap_or(100).max(1) as usize;
        ctx.check_external(&root)?;
        ctx.permit("grep", vec![pattern.to_string()], vec!["*".into()])?;
        // The search shows file contents, so a file the model names is a `read` as well, checked as the read tool
        // checks it (`grep . .env` asks like `read .env`). A root reached through a link (`safe.txt -> .env`, a linked
        // directory) is authorized as what it leads to, then searched at that resolved place. A plain directory root
        // needs no read check: what the walk finds in it is checked file by file below.
        let real = real_path(&root);
        let named_file = real.is_file();
        if named_file || (real != normalize(&root) && ctx.display(&real) != ctx.display(&root)) {
            ctx.permit_path("read", &root, vec!["*".into()])?;
        }
        let root = real;
        let protection = ReadProtection::new(ctx);
        let src = if opt_bool(input, "literal").unwrap_or(false) { regex::escape(pattern) } else { pattern.to_string() };
        let re = RegexBuilder::new(&src)
            .case_insensitive(!opt_bool(input, "caseSensitive").unwrap_or(true))
            .build()
            .map_err(|e| ToolError::fail(format!("invalid regex: {e}")))?;
        let include = match opt_str(input, "include") {
            Some(g) => Some(globset::Glob::new(g).map_err(|e| ToolError::fail(format!("invalid include glob: {e}")))?.compile_matcher()),
            None => None,
        };

        let mut out: Vec<String> = vec![];
        let mut total = 0usize;
        let mut skipped = 0usize;
        let mut withheld: Vec<String> = vec![];
        // Stop counting precisely after this many matches; report "N+".
        let count_cap = limit.saturating_mul(COUNT_FACTOR);
        let mut capped = false;

        // Stream the walk (sorted per directory, so output is deterministic) and
        // check for cancellation on every entry instead of collecting first.
        let mut builder = WalkBuilder::new(&root);
        builder.hidden(true).git_ignore(true).git_global(true).git_exclude(true).require_git(false);
        builder.sort_by_file_path(|a, b| a.cmp(b));
        'files: for entry in builder.build() {
            if ctx.cancelled() {
                return Err(ToolError::Interrupted);
            }
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            let rel = path.strip_prefix(&root).unwrap_or(path).to_path_buf();
            if let Some(inc) = &include {
                if !(inc.is_match(&rel) || path.file_name().is_some_and(|n| inc.is_match(n))) {
                    continue;
                }
            }
            // Found by the walk, not named by the model: a file the read rules protect (`prod.env`) is left out and
            // listed instead of asked about in the middle of a search. Grepping or reading it by path asks.
            if !named_file && protection.covers(path) {
                withheld.push(rel.display().to_string());
                continue;
            }
            let Ok(file) = std::fs::File::open(path) else { continue };
            // Data dumps and logs of this size are not what grep is for here, and would take minutes.
            if file.metadata().is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
                skipped += 1;
                continue;
            }
            let mut reader = BufReader::new(file);
            let mut buf = Vec::new();
            let mut n = 0usize;
            loop {
                if n.is_multiple_of(1024) && ctx.cancelled() {
                    return Err(ToolError::Interrupted);
                }
                // A line is matched on its first MAX_LINE_BYTES only: a minified bundle is one huge line.
                match super::fsio::read_line_capped(&mut reader, &mut buf, MAX_LINE_BYTES) {
                    Ok((0, _)) | Err(_) => break,
                    Ok(_) => {}
                }
                n += 1;
                if buf.contains(&0) {
                    break; // binary file
                }
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim_end_matches(['\n', '\r']);
                if re.is_match(line) {
                    total += 1;
                    if out.len() < limit {
                        let shown: String = line.chars().take(300).collect();
                        let shown = if line.chars().count() > 300 { format!("{shown}...") } else { shown };
                        out.push(format!("{}:{n}: {}", rel.display(), shown.trim_start()));
                    }
                    if total >= count_cap {
                        capped = true;
                        break 'files;
                    }
                }
            }
        }
        let mut notes = vec![];
        if skipped > 0 {
            notes.push(format!("[{skipped} file(s) over {} MiB were skipped; search them with the shell tool if needed]", MAX_FILE_BYTES / (1024 * 1024)));
        }
        if !withheld.is_empty() {
            let more = if withheld.len() > 5 { format!(" and {} more", withheld.len() - 5) } else { String::new() };
            notes.push(format!(
                "[{} file(s) were not searched because reading them needs permission: {}{more}. Grep or read one by its path to ask for it.]",
                withheld.len(),
                withheld[..withheld.len().min(5)].join(", ")
            ));
        }
        let note = notes.join("\n");
        if out.is_empty() {
            return Ok(ToolOutput::text(if note.is_empty() { "No matches found".to_string() } else { format!("No matches found\n\n{note}") }));
        }
        let mut text = out.join("\n");
        if total > limit {
            let plus = if capped { "+" } else { "" };
            text.push_str(&format!("\n\n[showing {limit} of {total}{plus} matches; narrow the pattern or raise limit]"));
        }
        if !note.is_empty() {
            text.push_str(&format!("\n\n{note}"));
        }
        Ok(ToolOutput { text, metadata: json!({"matches": total}) })
    }
}

/// The files the built-in read rules protect (`*.env`, `*.env.*`, not `*.env.example`), judged by the name the read
/// tool's check would give them (relative to the working directory or project, as `ToolCtx::permit_path` names them).
/// A walk cannot ask the permitter without prompting, so rules from config are applied only to paths the model names.
struct ReadProtection {
    rules: Vec<crate::permission::Rule>,
    bases: Vec<PathBuf>,
}

impl ReadProtection {
    fn new(ctx: &ToolCtx) -> Self {
        use crate::permission::{default_rules, Effect, Rule};
        let mut rules = vec![Rule::new("read", "*", Effect::Allow)];
        rules.extend(default_rules().into_iter().filter(|r| r.action == "read" && r.resource != "*"));
        let mut bases: Vec<PathBuf> = vec![];
        for b in [&ctx.cwd, &ctx.project_root] {
            for b in [normalize(b), real_path(b)] {
                if !bases.contains(&b) {
                    bases.push(b);
                }
            }
        }
        ReadProtection { rules, bases }
    }

    fn covers(&self, path: &Path) -> bool {
        let rel = self.bases.iter().find_map(|b| path.strip_prefix(b).ok().filter(|r| !r.as_os_str().is_empty())).unwrap_or(path);
        crate::permission::evaluate("read", &rel.to_string_lossy(), &self.rules) != crate::permission::Effect::Allow
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::testutil::Env;
    use super::*;
    use crate::permission::{decide, mode_rules, Effect, PermissionMode, Rule};
    use std::sync::Mutex;

    /// Decides as the engine does for a mode's rules (plus `extra`), records every request that would prompt and
    /// refuses it, so a test sees what the user would have been asked.
    pub(in crate::tools) struct Rules {
        rules: Vec<Rule>,
        asked: Mutex<Vec<String>>,
    }
    impl Rules {
        pub(in crate::tools) fn new(mode: PermissionMode, extra: Vec<Rule>) -> Self {
            let mut rules = mode_rules(mode);
            rules.extend(extra);
            Rules { rules, asked: Mutex::new(vec![]) }
        }
        pub(in crate::tools) fn auto() -> Self {
            Rules::new(PermissionMode::Auto, vec![])
        }
        /// The prompts, as `action resource, resource`.
        pub(in crate::tools) fn prompts(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }
    impl Permitter for Rules {
        fn check(&self, req: &PermReq) -> Result<(), ToolError> {
            match decide(&req.action, &req.resources, &self.rules, &[]) {
                Effect::Allow => Ok(()),
                Effect::Deny => Err(ToolError::Blocked(format!("Permission denied: {} {}", req.action, req.resources.join(", ")))),
                Effect::Ask => {
                    self.asked.lock().unwrap().push(format!("{} {}", req.action, req.resources.join(", ")));
                    Err(ToolError::Declined)
                }
            }
        }
    }

    fn run_with(env: &Env, perm: &Rules, v: Value) -> Result<String, ToolError> {
        Grep.run(&v, &ToolCtx { permitter: perm, ..env.ctx() }).map(|o| o.text)
    }

    #[test]
    fn a_protected_file_named_as_the_root_is_a_read_and_asks() {
        let env = Env::new();
        env.file(".env", b"DB_PASSWORD=hunter2\n");
        env.file("conf/prod.env", b"API_KEY=sk-live\n");
        env.file("notes.txt", b"DB_PASSWORD is in .env\n");
        for mode in [PermissionMode::Auto, PermissionMode::Manual] {
            let perm = Rules::new(mode, vec![]);
            assert_eq!(run_with(&env, &perm, json!({"pattern":".","path":".env"})), Err(ToolError::Declined), "{mode:?}");
            assert_eq!(run_with(&env, &perm, json!({"pattern":".","path":"conf/prod.env"})), Err(ToolError::Declined), "{mode:?}");
            assert_eq!(perm.prompts(), ["read .env", "read conf/prod.env"], "{mode:?}");
            // An ordinary file is still searched without a prompt.
            assert!(run_with(&env, &perm, json!({"pattern":"DB_","path":"notes.txt"})).unwrap().ends_with(":1: DB_PASSWORD is in .env"));
            assert_eq!(perm.prompts().len(), 2, "{mode:?}");
        }
        // Allowed by the user's rules: searched as before.
        let perm = Rules::new(PermissionMode::Auto, vec![Rule::new("read", "*.env", Effect::Allow)]);
        assert_eq!(run_with(&env, &perm, json!({"pattern":"DB_","path":".env"})).unwrap(), ":1: DB_PASSWORD=hunter2");
        // Denied by them: blocked.
        let perm = Rules::new(PermissionMode::Auto, vec![Rule::new("read", "notes.txt", Effect::Deny)]);
        assert!(matches!(run_with(&env, &perm, json!({"pattern":"DB_","path":"notes.txt"})), Err(ToolError::Blocked(_))));
    }

    #[test]
    fn a_tree_walk_leaves_out_protected_files_and_says_so() {
        let env = Env::new();
        env.file("conf/prod.env", b"SECRET=1\n");
        env.file("app.env.local", b"SECRET=2\n");
        env.file("x.env.d/inner.txt", b"SECRET=3\n");
        env.file("app.env.example", b"SECRET=example\n");
        env.file("src/main.rs", b"// SECRET=4 is not a secret\n");
        let perm = Rules::auto();
        let t = run_with(&env, &perm, json!({"pattern":"SECRET"})).unwrap();
        assert!(t.starts_with("app.env.example:1: SECRET=example\nsrc/main.rs:1: // SECRET=4"), "{t}");
        assert!(!t.contains("SECRET=1") && !t.contains("SECRET=2") && !t.contains("SECRET=3"), "{t}");
        assert!(t.contains("[3 file(s) were not searched because reading them needs permission: app.env.local, conf/prod.env, x.env.d/inner.txt."), "{t}");
        assert!(perm.prompts().is_empty(), "a walk does not prompt: {:?}", perm.prompts());
        // Also under a subdirectory root, and when the include glob names them.
        let t = run_with(&env, &perm, json!({"pattern":"SECRET","path":"conf","include":"*.env"})).unwrap();
        assert!(t.starts_with("No matches found") && t.contains("needs permission: prod.env."), "{t}");
        assert!(perm.prompts().is_empty());
    }

    fn run(env: &Env, v: Value) -> Result<String, ToolError> {
        Grep.run(&v, &env.ctx()).map(|o| o.text)
    }

    #[test]
    fn a_huge_single_line_is_searched_by_its_start_and_the_next_lines_still_count() {
        let env = Env::new();
        let mut data = b"needle ".to_vec();
        data.extend(std::iter::repeat(b'x').take(300_000));
        data.extend_from_slice(b"\nsecond needle\n");
        env.file("min.js", &data);
        let t = run(&env, json!({"pattern":"needle"})).unwrap();
        assert!(t.starts_with("min.js:1: needle xxx") && t.contains("min.js:2: second needle"), "{}", &t[..t.len().min(100)]);
    }

    #[test]
    fn regex_literal_case_include() {
        let env = Env::new();
        env.file("a.rs", b"fn main() {}\nlet X = 1;\n");
        env.file("b.txt", b"fn other\n");
        env.file("bin.dat", b"fn\0binary\n");
        assert_eq!(run(&env, json!({"pattern":"fn \\w+"})).unwrap(), "a.rs:1: fn main() {}\nb.txt:1: fn other");
        assert_eq!(run(&env, json!({"pattern":"fn \\w+","include":"*.rs"})).unwrap(), "a.rs:1: fn main() {}");
        assert_eq!(run(&env, json!({"pattern":"x = 1","caseSensitive":false})).unwrap(), "a.rs:2: let X = 1;");
        assert_eq!(run(&env, json!({"pattern":"x = 1"})).unwrap(), "No matches found");
        assert_eq!(run(&env, json!({"pattern":"main()","literal":true})).unwrap(), "a.rs:1: fn main() {}");
        assert!(run(&env, json!({"pattern":"("})).unwrap_err().message().contains("invalid regex"));
    }

    #[test]
    fn limit_and_gitignore() {
        let env = Env::new();
        env.file(".gitignore", b"skip.txt\n");
        env.file("skip.txt", b"hit\n");
        env.file("many.txt", "hit\n".repeat(5).as_bytes());
        let t = run(&env, json!({"pattern":"hit","limit":2})).unwrap();
        assert!(!t.contains("skip.txt") && t.contains("[showing 2 of 5 matches"), "{t}");
    }

    #[test]
    fn caps_count_and_single_file_root() {
        let env = Env::new();
        env.file("many.txt", "hit\n".repeat(50).as_bytes());
        let t = run(&env, json!({"pattern":"hit","limit":2})).unwrap();
        assert!(t.contains("[showing 2 of 20+ matches"), "{t}");
        let t = run(&env, json!({"pattern":"hit","path":"many.txt","limit":100})).unwrap();
        assert_eq!(t.lines().count(), 50);
    }
}
