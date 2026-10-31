use super::*;
use ignore::WalkBuilder;

pub struct Glob;

fn walk(root: &Path, hidden: bool) -> ignore::Walk {
    WalkBuilder::new(root).hidden(!hidden).git_ignore(true).git_global(true).git_exclude(true).require_git(false).build()
}

impl Tool for Glob {
    fn name(&self) -> &'static str {
        "glob"
    }
    fn description(&self) -> String {
        "Search file paths using a glob pattern (examples: \"**/*.ts\", \"src/**/*.tsx\"). Respects .gitignore.".into()
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "pattern":{"type":"string"},"path":{"type":"string","description":"Directory to search (default: working directory)"},
            "hidden":{"type":"boolean","description":"Include hidden files (default false)"},
            "limit":{"type":"integer","minimum":1,"description":"Max results (default 100)"}},"required":["pattern"]})
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let pattern = str_arg(input, "pattern")?;
        let root = opt_str(input, "path").map(|p| ctx.resolve(p)).unwrap_or_else(|| ctx.cwd.clone());
        let limit = opt_u64(input, "limit").unwrap_or(100).max(1) as usize;
        ctx.check_external(&root)?;
        ctx.permit("glob", vec![pattern.to_string()], vec!["*".into()])?;
        let matcher = globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| ToolError::fail(format!("invalid glob: {e}")))?
            .compile_matcher();
        let bare = !pattern.contains('/');
        let mut found = vec![];
        for entry in walk(&root, opt_bool(input, "hidden").unwrap_or(false)).filter_map(|e| e.ok()) {
            if ctx.cancelled() {
                return Err(ToolError::Interrupted);
            }
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            if matcher.is_match(rel) || (bare && entry.path().file_name().is_some_and(|n| matcher.is_match(n))) {
                found.push(rel.to_string_lossy().into_owned());
            }
        }
        found.sort();
        let total = found.len();
        found.truncate(limit);
        if found.is_empty() {
            return Ok(ToolOutput::text("No files found"));
        }
        let mut text = found.join("\n");
        if total > limit {
            text.push_str(&format!("\n\n[showing {limit} of {total} matches; narrow the pattern or raise limit]"));
        }
        Ok(ToolOutput { text, metadata: json!({"count": total}) })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::Env;
    use super::*;

    fn run(env: &Env, v: Value) -> String {
        Glob.run(&v, &env.ctx()).unwrap().text
    }

    #[test]
    fn matches_nested_and_bare_patterns() {
        let env = Env::new();
        for f in ["a.ts", "src/b.ts", "src/deep/c.ts", "src/d.rs", ".hidden/e.ts"] {
            env.file(f, b"");
        }
        assert_eq!(run(&env, json!({"pattern":"**/*.ts"})), "a.ts\nsrc/b.ts\nsrc/deep/c.ts");
        assert_eq!(run(&env, json!({"pattern":"*.ts"})), "a.ts\nsrc/b.ts\nsrc/deep/c.ts", "bare pattern matches basenames");
        assert_eq!(run(&env, json!({"pattern":"src/*.ts"})), "src/b.ts");
        assert!(run(&env, json!({"pattern":"**/*.ts","hidden":true})).contains(".hidden/e.ts"));
        assert_eq!(run(&env, json!({"pattern":"*.zz"})), "No files found");
        assert_eq!(run(&env, json!({"pattern":"**/*.ts","path":"undefined"})), "a.ts\nsrc/b.ts\nsrc/deep/c.ts");
    }

    #[test]
    fn respects_gitignore_and_limit() {
        let env = Env::new();
        env.file(".gitignore", b"ignored/\n");
        env.file("ignored/x.ts", b"");
        for i in 0..5 {
            env.file(&format!("f{i}.ts"), b"");
        }
        let t = run(&env, json!({"pattern":"*.ts","limit":2}));
        assert!(!t.contains("ignored"));
        assert!(t.contains("[showing 2 of 5 matches"), "{t}");
    }
}
