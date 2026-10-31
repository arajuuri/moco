use super::*;

pub struct Read;
const MAX_LINE: usize = 2000;

impl Tool for Read {
    fn name(&self) -> &'static str {
        "read"
    }
    fn description(&self) -> &'static str {
        "Read the contents of a file or directory. Supports text files; lines are prefixed with their line number. \
Prefer one larger read over many small slices, and use grep to find specific content in large files."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "path":{"type":"string","description":"File or directory, relative to the working directory or absolute"},
            "offset":{"type":"integer","minimum":1,"description":"1-based line (or directory entry) to start from"},
            "limit":{"type":"integer","minimum":1,"maximum":2000,"description":"Max lines or entries (default 2000)"}},
            "required":["path"]})
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let mut path = ctx.resolve(str_arg(input, "path")?);
        if !path.exists() {
            path = fuzzy_sibling(&path).ok_or_else(|| ToolError::fail(format!("File not found: {}", path.display())))?;
        }
        let dir = if path.is_dir() { path.clone() } else { path.parent().unwrap_or(&path).to_path_buf() };
        ctx.check_external(&dir)?;
        ctx.permit("read", vec![ctx.display(&path)], vec!["*".into()])?;

        let offset = opt_u64(input, "offset").unwrap_or(1).max(1) as usize;
        let limit = opt_u64(input, "limit").unwrap_or(2000).clamp(1, 2000) as usize;

        if path.is_dir() {
            let mut names: Vec<String> = std::fs::read_dir(&path)
                .map_err(|e| ToolError::fail(e.to_string()))?
                .filter_map(|e| e.ok())
                .map(|e| format!("{}{}", e.file_name().to_string_lossy(), if e.path().is_dir() { "/" } else { "" }))
                .collect();
            names.sort();
            if offset > names.len().max(1) {
                return Err(ToolError::fail(format!("offset {offset} is beyond the end ({} entries)", names.len())));
            }
            let page: Vec<_> = names.iter().skip(offset - 1).take(limit).cloned().collect();
            return Ok(ToolOutput::text(page.join("\n")));
        }

        let bytes = std::fs::read(&path).map_err(|e| ToolError::fail(format!("{}: {e}", path.display())))?;
        if bytes[..bytes.len().min(8192)].contains(&0) {
            return Err(ToolError::fail(format!("{} is a binary file; use shell to inspect it", path.display())));
        }
        let text = String::from_utf8_lossy(&bytes);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
        // A trailing newline yields an empty final element that is not a real line.
        let total = if lines.last() == Some(&"") && lines.len() > 1 { lines.len() - 1 } else { lines.len() };
        if offset > total {
            return Err(ToolError::fail(format!("offset {offset} is beyond the end of the file ({total} lines)")));
        }
        let mut out = String::new();
        for (i, line) in lines.iter().take(total).enumerate().skip(offset - 1).take(limit) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("{}: ", i + 1));
            if line.chars().count() > MAX_LINE {
                out.extend(line.chars().take(MAX_LINE));
                out.push_str("... (line truncated to 2000 chars)");
            } else {
                out.push_str(line);
            }
        }
        let end = (offset - 1 + limit).min(total);
        if end < total {
            out.push_str(&format!("\n\n[showing lines {offset}-{end} of {total}; use offset={} to continue]", end + 1));
        }
        Ok(ToolOutput { text: out, metadata: json!({"path": path.to_string_lossy(), "lines": total}) })
    }
}

/// macOS screenshot names differ from what models type: NBSP/narrow NBSP vs space, curly quotes, NFC vs NFD.
fn fuzzy_sibling(missing: &Path) -> Option<PathBuf> {
    fn key(s: &str) -> String {
        s.chars()
            .map(|c| match c {
                '\u{a0}' | '\u{202f}' => ' ',
                '\u{2018}' | '\u{2019}' => '\'',
                '\u{201c}' | '\u{201d}' => '"',
                c => c,
            })
            .collect()
    }
    let want = key(&nfc(&missing.file_name()?.to_string_lossy()));
    let mut hits = std::fs::read_dir(missing.parent()?).ok()?.filter_map(|e| e.ok()).filter(|e| key(&nfc(&e.file_name().to_string_lossy())) == want);
    let first = hits.next()?;
    if hits.next().is_some() { None } else { Some(first.path()) }
}

/// Minimal NFC for the common case of a base letter + combining mark (no unicode-normalization dependency).
fn nfc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let composed = match (out.chars().last(), c) {
            (Some(b), '\u{301}') => compose(b, 'a', 'á', 'e', 'é', 'i', 'í', 'o', 'ó', 'u', 'ú'),
            (Some(b), '\u{308}') => compose(b, 'a', 'ä', 'e', 'ë', 'i', 'ï', 'o', 'ö', 'u', 'ü'),
            _ => None,
        };
        match composed {
            Some(n) => {
                out.pop();
                out.push(n);
            }
            None => out.push(c),
        }
    }
    out
}
#[allow(clippy::too_many_arguments)]
fn compose(b: char, a: char, a2: char, e: char, e2: char, i: char, i2: char, o: char, o2: char, u: char, u2: char) -> Option<char> {
    [(a, a2), (e, e2), (i, i2), (o, o2), (u, u2)].iter().find(|(x, _)| *x == b).map(|(_, y)| *y)
}

#[cfg(test)]
mod tests {
    use super::super::testutil::Env;
    use super::*;

    fn run(env: &Env, v: Value) -> Result<ToolOutput, ToolError> {
        Read.run(&v, &env.ctx())
    }

    #[test]
    fn numbered_lines_offset_limit() {
        let env = Env::new();
        env.file("a.txt", b"one\ntwo\nthree\n");
        assert_eq!(run(&env, json!({"path":"a.txt"})).unwrap().text, "1: one\n2: two\n3: three");
        let t = run(&env, json!({"path":"a.txt","offset":2,"limit":1})).unwrap().text;
        assert!(t.starts_with("2: two") && t.contains("[showing lines 2-2 of 3; use offset=3"), "{t}");
        assert!(run(&env, json!({"path":"a.txt","offset":9})).is_err());
        assert_eq!(env.perm.asked()[0].resources, ["a.txt"]);
    }

    #[test]
    fn long_lines_bom_crlf() {
        let env = Env::new();
        env.file("b.txt", b"\xef\xbb\xbfhi\r\nyo\r\n");
        assert_eq!(run(&env, json!({"path":"b.txt"})).unwrap().text, "1: hi\n2: yo");
        env.file("c.txt", "x".repeat(2500).as_bytes());
        assert!(run(&env, json!({"path":"c.txt"})).unwrap().text.ends_with("... (line truncated to 2000 chars)"));
    }

    #[test]
    fn directory_and_binary_and_missing() {
        let env = Env::new();
        env.file("d/x.txt", b"x");
        env.file("d/sub/y.txt", b"y");
        assert_eq!(run(&env, json!({"path":"d"})).unwrap().text, "sub/\nx.txt");
        env.file("bin", b"ab\0cd");
        assert!(run(&env, json!({"path":"bin"})).unwrap_err().message().contains("binary"));
        assert!(run(&env, json!({"path":"nope"})).unwrap_err().message().contains("File not found"));
    }

    #[test]
    fn fuzzy_screenshot_name() {
        let env = Env::new();
        env.file("Screenshot 2024 at 10.00.00\u{202f}AM.txt", b"shot");
        let t = run(&env, json!({"path":"Screenshot 2024 at 10.00.00 AM.txt"})).unwrap();
        assert_eq!(t.text, "1: shot");
    }

    #[test]
    fn external_paths_ask_first() {
        let env = Env::new();
        let outside = env.dir.join("outside.txt");
        std::fs::write(&outside, "o").unwrap();
        run(&env, json!({"path": outside.to_string_lossy()})).unwrap();
        let asked = env.perm.asked();
        assert_eq!(asked[0].action, "external_directory");
        assert_eq!(asked[0].resources, [format!("{}/*", env.dir.display())]);
        assert_eq!(asked[1].action, "read");
        assert_eq!(asked[1].resources, [outside.to_string_lossy()]);
    }

    #[test]
    fn denied_permission_propagates() {
        let env = Env::new();
        env.file("a.txt", b"x");
        *env.perm.reply.lock().unwrap() = Err(ToolError::Declined);
        assert_eq!(run(&env, json!({"path":"a.txt"})).unwrap_err(), ToolError::Declined);
    }
}
