//! `patch`: multi-file edits in the `*** Begin Patch` format (docs/04-tools.md).
use super::*;
use std::collections::HashMap;

pub struct Patch;

#[derive(Debug, Clone, PartialEq)]
pub enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Hunk {
    /// Text after `@@`: a line to locate before applying the hunk.
    pub context: Option<String>,
    pub lines: Vec<HunkLine>,
    /// `*** End of File`: the hunk must match at the end of the file.
    pub eof: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Add { path: String, lines: Vec<String> },
    Delete { path: String },
    Update { path: String, move_to: Option<String>, hunks: Vec<Hunk> },
}

const HEADERS: &[&str] = &["*** Add File: ", "*** Delete File: ", "*** Update File: "];

fn is_header(l: &str) -> bool {
    HEADERS.iter().any(|h| l.starts_with(h)) || l == "*** End Patch"
}

pub fn parse(text: &str) -> Result<Vec<Op>, String> {
    let lines: Vec<&str> = text.trim().lines().collect();
    if lines.first().map(|l| l.trim()) != Some("*** Begin Patch") {
        return Err("patch must start with '*** Begin Patch'".into());
    }
    if lines.last().map(|l| l.trim()) != Some("*** End Patch") {
        return Err("patch must end with '*** End Patch'".into());
    }
    let end = lines.len() - 1;
    let mut ops = vec![];
    let mut i = 1;
    while i < end {
        let l = lines[i];
        if let Some(path) = l.strip_prefix("*** Add File: ") {
            let mut content = vec![];
            i += 1;
            while i < end && !is_header(lines[i]) {
                content.push(lines[i].strip_prefix('+').ok_or_else(|| format!("Add File {path}: every line must start with '+' (got '{}')", lines[i]))?.to_string());
                i += 1;
            }
            ops.push(Op::Add { path: path.trim().into(), lines: content });
        } else if let Some(path) = l.strip_prefix("*** Delete File: ") {
            ops.push(Op::Delete { path: path.trim().into() });
            i += 1;
        } else if let Some(path) = l.strip_prefix("*** Update File: ") {
            i += 1;
            let move_to = lines.get(i).and_then(|m| m.strip_prefix("*** Move to: ")).map(|m| m.trim().to_string());
            if move_to.is_some() {
                i += 1;
            }
            let mut hunks: Vec<Hunk> = vec![];
            let mut cur: Option<Hunk> = None;
            while i < end && (!is_header(lines[i])) {
                let h = lines[i];
                if let Some(ctx) = h.strip_prefix("@@") {
                    hunks.extend(cur.take());
                    let ctx = ctx.trim();
                    cur = Some(Hunk { context: (!ctx.is_empty()).then(|| ctx.to_string()), ..Default::default() });
                } else if h == "*** End of File" {
                    cur.get_or_insert_with(Hunk::default).eof = true;
                } else {
                    let hunk = cur.get_or_insert_with(Hunk::default);
                    hunk.lines.push(match h.chars().next() {
                        Some(' ') => HunkLine::Context(h[1..].to_string()),
                        Some('-') => HunkLine::Remove(h[1..].to_string()),
                        Some('+') => HunkLine::Add(h[1..].to_string()),
                        None => HunkLine::Context(String::new()), // models often drop the leading space of blank lines
                        Some(_) => return Err(format!("Update File {path}: hunk lines must start with ' ', '-' or '+' (got '{h}')")),
                    });
                }
                i += 1;
            }
            hunks.extend(cur);
            if hunks.is_empty() && move_to.is_none() {
                return Err(format!("Update File {path}: no changes"));
            }
            ops.push(Op::Update { path: path.trim().into(), move_to, hunks });
        } else {
            return Err(format!("unexpected line in patch: '{l}'"));
        }
    }
    if ops.is_empty() {
        return Err("patch contains no changes".into());
    }
    Ok(ops)
}

type Eq = fn(&str, &str) -> bool;
const LEVELS: [Eq; 4] = [
    |a, b| a == b,
    |a, b| a.trim_end() == b.trim_end(),
    |a, b| a.trim() == b.trim(),
    |a, b| a.trim().chars().map(edit::fold).eq(b.trim().chars().map(edit::fold)),
];

/// First index `>= start` where `pat` matches, trying exact matching first and loosening step by step.
fn find_seq(lines: &[String], pat: &[&str], start: usize, eof: bool) -> Option<usize> {
    if pat.len() > lines.len() {
        return None;
    }
    let last = lines.len() - pat.len();
    for eq in LEVELS {
        let ok = |i: usize| pat.iter().enumerate().all(|(k, p)| eq(&lines[i + k], p));
        if eof {
            if last >= start && ok(last) {
                return Some(last);
            }
        } else if let Some(i) = (start..=last).find(|&i| ok(i)) {
            return Some(i);
        }
    }
    None
}

/// Apply hunks to file text. Context lines keep the file's own text; CRLF and the trailing newline are preserved.
pub fn apply(content: &str, hunks: &[Hunk], path: &str) -> Result<String, String> {
    let crlf = content.contains("\r\n");
    let norm = if crlf { content.replace("\r\n", "\n") } else { content.to_string() };
    let trailing = norm.ends_with('\n');
    let body = norm.strip_suffix('\n').unwrap_or(&norm);
    let mut lines: Vec<String> = if body.is_empty() && !trailing { vec![] } else { body.split('\n').map(String::from).collect() };
    let mut pos = 0;
    for h in hunks {
        if let Some(ctx) = &h.context {
            let i = find_seq(&lines, &[ctx.as_str()], pos, false).ok_or_else(|| format!("Failed to find context line '{ctx}' in {path}"))?;
            pos = i + 1;
        }
        let old: Vec<&str> = h.lines.iter().filter_map(|l| match l {
            HunkLine::Context(s) | HunkLine::Remove(s) => Some(s.as_str()),
            HunkLine::Add(_) => None,
        }).collect();
        let at = if old.is_empty() {
            if h.eof || h.context.is_none() { lines.len() } else { pos }
        } else {
            find_seq(&lines, &old, pos, h.eof).ok_or_else(|| {
                format!("Failed to find expected lines in {path}:\n{}\nRe-read the file and make the hunk match its current content.", old.join("\n"))
            })?
        };
        let mut replacement = vec![];
        let mut k = at;
        for l in &h.lines {
            match l {
                HunkLine::Context(_) => {
                    replacement.push(lines[k].clone());
                    k += 1;
                }
                HunkLine::Remove(_) => k += 1,
                HunkLine::Add(s) => replacement.push(s.clone()),
            }
        }
        let n = replacement.len();
        lines.splice(at..k, replacement);
        pos = at + n;
    }
    let mut out = lines.join("\n");
    if trailing || (!out.is_empty() && content.is_empty()) {
        out.push('\n');
    }
    Ok(if crlf { out.replace('\n', "\r\n") } else { out })
}

const BOM: [u8; 3] = [0xef, 0xbb, 0xbf];

impl Tool for Patch {
    fn name(&self) -> &'static str {
        "patch"
    }
    fn description(&self) -> String {
        "Apply a multi-file patch atomically. Format:\n*** Begin Patch\n*** Add File: <path>  (then every line prefixed '+')\n*** Delete File: <path>\n\
*** Update File: <path>  (optional '*** Move to: <new path>', then hunks starting with '@@ [context line]' followed by ' ' context, '-' removed and '+' added lines)\n\
*** End Patch\nAll hunks are validated before anything is written. Prefer edit for a single small change."
            .into()
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"patchText":{"type":"string","description":"The full patch, from '*** Begin Patch' to '*** End Patch'"}},"required":["patchText"]})
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let ops = parse(str_arg(input, "patchText")?).map_err(ToolError::fail)?;

        // Resolve and permission-check every path before touching anything.
        let mut resources = vec![];
        let mut files: Vec<PathBuf> = vec![];
        for op in &ops {
            let paths: Vec<&String> = match op {
                Op::Add { path, .. } | Op::Delete { path } => vec![path],
                Op::Update { path, move_to, .. } => std::iter::once(path).chain(move_to).collect(),
            };
            for p in paths {
                let abs = ctx.resolve(p);
                ctx.check_external_file(&abs)?;
                resources.push(ctx.display(&abs));
                // A link in the path (final component or parent) is authorized as what it leads to as well.
                resources.push(ctx.display(&real_path(&abs)));
                files.push(abs);
            }
        }
        let mut seen = std::collections::HashSet::new();
        resources.retain(|r| seen.insert(r.clone()));
        ctx.permit("edit", resources, vec!["*".into()])?;
        // Every file this patch touches is ours until it is written and formatted (issue #6). Taken after the
        // prompt, which can take minutes, and before the first read.
        let _locks = super::fsio::lock_files(&files);

        // Stage the result of every op in memory; later ops see earlier ones.
        let mut staged: HashMap<PathBuf, Option<Vec<u8>>> = HashMap::new();
        let mut order: Vec<PathBuf> = vec![];
        let mut summary: Vec<String> = vec![];
        let read = |staged: &HashMap<PathBuf, Option<Vec<u8>>>, p: &Path| -> Result<Option<Vec<u8>>, ToolError> {
            match staged.get(p) {
                Some(v) => Ok(v.clone()),
                None => match std::fs::read(p) {
                    Ok(b) => Ok(Some(b)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(ToolError::fail(format!("{}: {e}", p.display()))),
                },
            }
        };
        let mut stage = |staged: &mut HashMap<PathBuf, Option<Vec<u8>>>, p: PathBuf, v: Option<Vec<u8>>| {
            if !order.contains(&p) {
                order.push(p.clone());
            }
            staged.insert(p, v);
        };
        for op in &ops {
            match op {
                Op::Add { path, lines } => {
                    let abs = real_path(&ctx.resolve(path));
                    if read(&staged, &abs)?.is_some() {
                        return Err(ToolError::fail(format!("Add File {path}: the file already exists; use Update File")));
                    }
                    let mut text = lines.join("\n");
                    if !lines.is_empty() {
                        text.push('\n');
                    }
                    stage(&mut staged, abs, Some(text.into_bytes()));
                    summary.push(format!("A {path}"));
                }
                Op::Delete { path } => {
                    let abs = real_path(&ctx.resolve(path));
                    if read(&staged, &abs)?.is_none() {
                        return Err(ToolError::fail(format!("Delete File {path}: no such file")));
                    }
                    stage(&mut staged, abs, None);
                    summary.push(format!("D {path}"));
                }
                Op::Update { path, move_to, hunks } => {
                    let abs = real_path(&ctx.resolve(path));
                    let bytes = read(&staged, &abs)?.ok_or_else(|| ToolError::fail(format!("Update File {path}: no such file")))?;
                    let (bom, body) = match bytes.strip_prefix(&BOM) {
                        Some(r) => (true, r),
                        None => (false, &bytes[..]),
                    };
                    let text = std::str::from_utf8(body).map_err(|_| ToolError::fail(format!("{path}: not valid UTF-8")))?;
                    let new_text = apply(text, hunks, path).map_err(ToolError::fail)?;
                    let mut out = if bom { BOM.to_vec() } else { vec![] };
                    out.extend_from_slice(new_text.as_bytes());
                    match move_to {
                        Some(dest) => {
                            let dest_abs = real_path(&ctx.resolve(dest));
                            if dest_abs != abs && read(&staged, &dest_abs)?.is_some() {
                                return Err(ToolError::fail(format!("Move to {dest}: the destination already exists")));
                            }
                            if dest_abs != abs {
                                stage(&mut staged, abs, None);
                            }
                            stage(&mut staged, dest_abs, Some(out));
                            summary.push(format!("M {dest}"));
                        }
                        None => {
                            stage(&mut staged, abs, Some(out));
                            summary.push(format!("M {path}"));
                        }
                    }
                }
            }
        }

        // Everything validated: write.
        for p in &order {
            match &staged[p] {
                Some(bytes) => {
                    if let Some(dir) = p.parent() {
                        std::fs::create_dir_all(dir).map_err(|e| ToolError::fail(e.to_string()))?;
                    }
                    super::fsio::write_atomic(p, bytes).map_err(|e| ToolError::fail(format!("{}: {e}", p.display())))?;
                }
                None => {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        // Format what was written (not what was deleted); say so for each file the formatter changed or choked on.
        let mut notes = vec![];
        for p in &order {
            if let Some(written) = &staged[p] {
                if let Some(n) = ctx.format_file(p, written) {
                    notes.push(format!("{}: {n}", ctx.display(p)));
                }
            }
        }
        let tail = if notes.is_empty() { String::new() } else { format!("\n\n{}", notes.join("\n")) };
        Ok(ToolOutput { text: format!("Success. Updated the following files:\n{}{tail}", summary.join("\n")), metadata: json!({"files": summary}) })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::Env;
    use super::*;

    fn run(env: &Env, patch: &str) -> Result<ToolOutput, ToolError> {
        Patch.run(&json!({ "patchText": patch }), &env.ctx())
    }
    fn wrap(body: &str) -> String {
        format!("*** Begin Patch\n{body}\n*** End Patch")
    }

    #[test]
    fn parses_all_three_operations() {
        let ops = parse(&wrap("*** Add File: a.txt\n+one\n+two\n*** Delete File: b.txt\n*** Update File: c.txt\n*** Move to: d.txt\n@@ fn main\n keep\n-old\n+new\n*** End of File")).unwrap();
        assert_eq!(ops.len(), 3);
        assert_eq!(ops[0], Op::Add { path: "a.txt".into(), lines: vec!["one".into(), "two".into()] });
        let Op::Update { move_to, hunks, .. } = &ops[2] else { panic!() };
        assert_eq!(move_to.as_deref(), Some("d.txt"));
        assert_eq!(hunks[0].context.as_deref(), Some("fn main"));
        assert!(hunks[0].eof);
        assert_eq!(hunks[0].lines, [HunkLine::Context("keep".into()), HunkLine::Remove("old".into()), HunkLine::Add("new".into())]);
    }

    #[test]
    fn rejects_malformed_patches() {
        assert!(parse("*** Update File: a\n").unwrap_err().contains("Begin Patch"));
        assert!(parse("*** Begin Patch\n*** Add File: a\n+x").unwrap_err().contains("End Patch"));
        assert!(parse(&wrap("*** Add File: a\nnot prefixed")).unwrap_err().contains("must start with '+'"));
        assert!(parse(&wrap("*** Update File: a\n@@\n?bad")).unwrap_err().contains("hunk lines"));
        assert!(parse(&wrap("*** Update File: a")).unwrap_err().contains("no changes"));
        assert!(parse(&wrap("junk")).unwrap_err().contains("unexpected line"));
        assert!(parse("*** Begin Patch\n*** End Patch").unwrap_err().contains("no changes"));
    }

    fn hunks(body: &str) -> Vec<Hunk> {
        let Op::Update { hunks, .. } = parse(&wrap(&format!("*** Update File: f\n{body}"))).unwrap().remove(0) else { panic!() };
        hunks
    }

    #[test]
    fn apply_with_context_and_multiple_hunks() {
        let file = "fn a() {\n    1\n}\n\nfn b() {\n    2\n}\n";
        let out = apply(file, &hunks("@@ fn a() {\n-    1\n+    10\n@@ fn b() {\n     2\n+    3\n }"), "f").unwrap();
        assert_eq!(out, "fn a() {\n    10\n}\n\nfn b() {\n    2\n    3\n}\n");
    }

    #[test]
    fn fuzzy_matching_keeps_the_files_own_context_text() {
        // Patch has different whitespace and curly quotes; the file's context lines survive untouched.
        let file = "let s = \u{201c}hi\u{201d};   \n    keep me\t\nend\n";
        let out = apply(file, &hunks(" let s = \"hi\";\n     keep me\n-end\n+END"), "f").unwrap();
        assert_eq!(out, "let s = \u{201c}hi\u{201d};   \n    keep me\t\nEND\n");
    }

    #[test]
    fn eof_hunks_pure_additions_and_missing_text() {
        assert_eq!(apply("a\nb\n", &hunks(" b\n+c\n*** End of File"), "f").unwrap(), "a\nb\nc\n");
        assert_eq!(apply("a\nb\n", &hunks("+tail"), "f").unwrap(), "a\nb\ntail\n", "no context: append");
        assert_eq!(apply("x\ny\n", &hunks("@@ x\n+between"), "f").unwrap(), "x\nbetween\ny\n", "context with only additions inserts after it");
        let e = apply("a\n", &hunks("-zzz\n+q"), "f").unwrap_err();
        assert!(e.contains("Failed to find expected lines in f") && e.contains("zzz"));
        assert!(apply("a\n", &hunks("@@ nope\n-a"), "f").unwrap_err().contains("context line 'nope'"));
    }

    #[test]
    fn crlf_and_missing_trailing_newline_are_preserved() {
        assert_eq!(apply("a\r\nb\r\n", &hunks("-a\n+A"), "f").unwrap(), "A\r\nb\r\n");
        assert_eq!(apply("a\nb", &hunks("-b\n+B"), "f").unwrap(), "a\nB");
        assert_eq!(apply("", &hunks("+first"), "f").unwrap(), "first\n");
    }

    #[test]
    fn tool_add_update_delete_move() {
        let env = Env::new();
        env.file("old.txt", b"\xef\xbb\xbfone\ntwo\n");
        env.file("gone.txt", b"bye\n");
        env.file("edit.txt", b"x\ny\n");
        let o = run(&env, &wrap("*** Add File: new/dir/n.txt\n+hello\n*** Delete File: gone.txt\n*** Update File: edit.txt\n@@\n-y\n+Y\n*** Update File: old.txt\n*** Move to: moved.txt\n@@\n one\n-two\n+2")).unwrap();
        assert_eq!(o.text, "Success. Updated the following files:\nA new/dir/n.txt\nD gone.txt\nM edit.txt\nM moved.txt");
        let w = env.work();
        assert_eq!(std::fs::read_to_string(w.join("new/dir/n.txt")).unwrap(), "hello\n");
        assert!(!w.join("gone.txt").exists() && !w.join("old.txt").exists());
        assert_eq!(std::fs::read_to_string(w.join("edit.txt")).unwrap(), "x\nY\n");
        assert_eq!(std::fs::read(w.join("moved.txt")).unwrap(), b"\xef\xbb\xbfone\n2\n", "BOM kept across the move");
        let asked = env.perm.asked();
        assert_eq!(asked[0].action, "edit");
        assert_eq!(asked[0].resources, ["new/dir/n.txt", "gone.txt", "edit.txt", "old.txt", "moved.txt"]);
    }

    #[test]
    fn patch_formats_written_files_but_not_deleted_ones() {
        let mut env = Env::new();
        env.formatters = crate::format::tests::upcase_formatters();
        env.file("old.txt", b"bye\n");
        env.file("edit.txt", b"keep\nchange\n");
        let o = run(&env, &wrap("*** Add File: new.txt\n+fresh\n*** Delete File: old.txt\n*** Update File: edit.txt\n@@\n keep\n-change\n+changed\n*** Add File: plain.rs\n+fn x() {}")).unwrap();
        let w = env.work();
        assert_eq!((std::fs::read_to_string(w.join("new.txt")).unwrap().as_str(), std::fs::read_to_string(w.join("edit.txt")).unwrap().as_str()), ("FRESH\n", "KEEP\nCHANGED\n"));
        assert_eq!(std::fs::read_to_string(w.join("plain.rs")).unwrap(), "fn x() {}\n");
        assert!(!w.join("old.txt").exists());
        assert!(o.text.contains("new.txt: Formatted with upcase") && o.text.contains("edit.txt: Formatted with upcase") && !o.text.contains("plain.rs: ") && !o.text.contains("old.txt: "), "{}", o.text);
    }

    #[test]
    fn patched_files_keep_their_mode_and_nothing_is_left_beside_them() {
        use std::os::unix::fs::PermissionsExt;
        let env = Env::new();
        let p = env.file("run.sh", b"#!/bin/sh\necho old\n");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        run(&env, "*** Begin Patch\n*** Update File: run.sh\n@@\n-echo old\n+echo new\n*** Add File: other.txt\n+hi\n*** End Patch").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "#!/bin/sh\necho new\n");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o755);
        let mut names: Vec<String> = std::fs::read_dir(env.work()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["other.txt", "run.sh"], "{names:?}");
    }

    /// A patch and an edit that touch the same file at the same time both land (issue #6).
    #[test]
    fn a_patch_and_edits_on_one_file_do_not_lose_each_other() {
        let env = Env::new();
        let body: String = (0..9).map(|i| format!("row{i}=old\n")).collect();
        let p = env.file("t.txt", body.as_bytes());
        std::thread::scope(|s| {
            for i in 0..8 {
                let env = &env;
                s.spawn(move || {
                    super::super::edit::Edit.run(&json!({"path":"t.txt","oldString":format!("row{i}=old"),"newString":format!("row{i}=new")}), &env.ctx()).unwrap();
                });
            }
            let env = &env;
            s.spawn(move || {
                run(env, "*** Begin Patch\n*** Update File: t.txt\n@@\n-row8=old\n+row8=new\n*** End Patch").unwrap();
            });
        });
        let after = std::fs::read_to_string(&p).unwrap();
        assert_eq!(after.matches("=new").count(), 9, "{after}");
    }

    #[test]
    fn a_bad_hunk_changes_nothing() {
        let env = Env::new();
        env.file("a.txt", b"keep\n");
        env.file("b.txt", b"real\n");
        let e = run(&env, &wrap("*** Update File: a.txt\n@@\n-keep\n+CHANGED\n*** Update File: b.txt\n@@\n-not there\n+x\n*** Add File: c.txt\n+c")).unwrap_err();
        assert!(e.message().contains("Failed to find expected lines in b.txt"));
        assert_eq!(std::fs::read_to_string(env.work().join("a.txt")).unwrap(), "keep\n");
        assert!(!env.work().join("c.txt").exists());
    }

    #[test]
    fn existence_rules_and_sequencing() {
        let env = Env::new();
        env.file("a.txt", b"x\n");
        assert!(run(&env, &wrap("*** Add File: a.txt\n+y")).unwrap_err().message().contains("already exists"));
        assert!(run(&env, &wrap("*** Delete File: nope.txt")).unwrap_err().message().contains("no such file"));
        assert!(run(&env, &wrap("*** Update File: nope.txt\n@@\n-a\n+b")).unwrap_err().message().contains("no such file"));
        env.file("dest.txt", b"d\n");
        assert!(run(&env, &wrap("*** Update File: a.txt\n*** Move to: dest.txt\n@@\n-x\n+z")).unwrap_err().message().contains("destination already exists"));
        // A later op sees an earlier one: add then update the same new file.
        run(&env, &wrap("*** Add File: s.txt\n+1\n+2\n*** Update File: s.txt\n@@\n 1\n-2\n+two")).unwrap();
        assert_eq!(std::fs::read_to_string(env.work().join("s.txt")).unwrap(), "1\ntwo\n");
    }

    #[test]
    fn denied_permission_writes_nothing() {
        let env = Env::new();
        *env.perm.reply.lock().unwrap() = Err(ToolError::Declined);
        assert_eq!(run(&env, &wrap("*** Add File: x.txt\n+x")).unwrap_err(), ToolError::Declined);
        assert!(!env.work().join("x.txt").exists());
    }
}
