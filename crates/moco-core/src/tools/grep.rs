use super::*;
use ignore::WalkBuilder;
use regex::RegexBuilder;
use std::io::{BufRead, BufReader};

pub struct Grep;

impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }
    fn description(&self) -> &'static str {
        "Search file contents with a regular expression (Rust regex syntax) or a literal string. Returns `path:line: text`. Respects .gitignore."
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
        let walker = WalkBuilder::new(&root).hidden(true).git_ignore(true).git_global(true).git_exclude(true).require_git(false).build();
        let mut files: Vec<PathBuf> = walker
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .map(|e| e.into_path())
            .collect();
        if root.is_file() {
            files = vec![root.clone()];
        }
        files.sort();
        for path in files {
            if ctx.cancelled() {
                return Err(ToolError::Interrupted);
            }
            let rel = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
            if let Some(inc) = &include {
                if !(inc.is_match(&rel) || path.file_name().is_some_and(|n| inc.is_match(n))) {
                    continue;
                }
            }
            let Ok(file) = std::fs::File::open(&path) else { continue };
            let mut reader = BufReader::new(file);
            let mut buf = Vec::new();
            let mut n = 0usize;
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
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
                }
            }
        }
        if out.is_empty() {
            return Ok(ToolOutput::text("No matches found"));
        }
        let mut text = out.join("\n");
        if total > limit {
            text.push_str(&format!("\n\n[showing {limit} of {total} matches; narrow the pattern or raise limit]"));
        }
        Ok(ToolOutput { text, metadata: json!({"matches": total}) })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::Env;
    use super::*;

    fn run(env: &Env, v: Value) -> Result<String, ToolError> {
        Grep.run(&v, &env.ctx()).map(|o| o.text)
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
}
