use super::*;

pub struct Edit;

impl Tool for Edit {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn description(&self) -> String {
        "Performs exact string replacement in a file. `oldString` must match exactly once unless `replaceAll` is set; \
include enough surrounding context to make it unique. Do not include the `N: ` line-number prefix shown by the read tool. \
`newString` must differ from `oldString`.".into()
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "path":{"type":"string"},"oldString":{"type":"string"},"newString":{"type":"string"},
            "replaceAll":{"type":"boolean","description":"Replace every occurrence"}},
            "required":["path","oldString","newString"]})
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = ctx.resolve(str_arg(input, "path")?);
        let (old, new) = (str_arg(input, "oldString")?, str_arg(input, "newString")?);
        let all = opt_bool(input, "replaceAll").unwrap_or(false);
        if old == new {
            return Err(ToolError::fail("newString must be different from oldString"));
        }
        ctx.check_external(path.parent().unwrap_or(&path))?;
        ctx.permit("edit", vec![ctx.display(&path)], vec!["*".into()])?;
        let bytes = std::fs::read(&path).map_err(|e| ToolError::fail(format!("{}: {e}", path.display())))?;
        let (bom, body) = match bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
            Some(rest) => (true, rest),
            None => (false, &bytes[..]),
        };
        let content = std::str::from_utf8(body).map_err(|_| ToolError::fail("file is not valid UTF-8"))?;

        // The model writes `\n`. For CRLF files convert, unless the model deliberately included `\r`.
        let crlf = content.contains("\r\n") && !old.contains('\r');
        let (old, new) = if crlf { (to_crlf(old), to_crlf(new)) } else { (old.to_string(), new.to_string()) };

        let (updated, n) = replace(content, &old, &new, all).map_err(ToolError::fail)?;
        let mut out = Vec::with_capacity(updated.len() + 3);
        if bom {
            out.extend_from_slice(&[0xef, 0xbb, 0xbf]);
        }
        out.extend_from_slice(updated.as_bytes());
        std::fs::write(&path, out).map_err(|e| ToolError::fail(e.to_string()))?;
        Ok(ToolOutput {
            text: format!("Edit applied: {n} replacement{} in {}", if n == 1 { "" } else { "s" }, ctx.display(&path)),
            metadata: json!({"path": path.to_string_lossy(), "replacements": n}),
        })
    }
}

fn to_crlf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
}

pub(crate) fn fold(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201b}' => '\'',
        '\u{201c}' | '\u{201d}' | '\u{201e}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{a0}' | '\u{202f}' | '\u{2009}' | '\u{200a}' | '\u{2002}' | '\u{2003}' => ' ',
        c => c,
    }
}

const NOT_FOUND: &str = "oldString not found in file. It must match the file content exactly (including whitespace); re-read the file and try again.";

/// Exact match, then typographic-variant match, then per-line trailing-whitespace-insensitive match.
pub fn replace(content: &str, old: &str, new: &str, all: bool) -> Result<(String, usize), String> {
    if old.is_empty() {
        return Err("oldString must not be empty".into());
    }
    let many = || "oldString matches more than once. Add surrounding context to make it unique, or set replaceAll.".to_string();

    let exact = content.matches(old).count();
    if exact == 1 || (exact > 1 && all) {
        return Ok((content.replace(old, new), exact));
    }
    if exact > 1 {
        return Err(many());
    }

    // (a) typographic variants: compare on folded chars; folding is 1:1 so char offsets line up.
    let c: Vec<char> = content.chars().collect();
    let cf: Vec<char> = c.iter().map(|&x| fold(x)).collect();
    let of: Vec<char> = old.chars().map(fold).collect();
    let mut hits = vec![];
    let mut i = 0;
    while i + of.len() <= cf.len() {
        if cf[i..i + of.len()] == of[..] {
            hits.push(i);
            i += of.len();
        } else {
            i += 1;
        }
    }
    if hits.len() == 1 || (hits.len() > 1 && all) {
        let mut out = String::new();
        let mut at = 0;
        for h in &hits {
            out.extend(&c[at..*h]);
            out.push_str(new);
            at = h + of.len();
        }
        out.extend(&c[at..]);
        return Ok((out, hits.len()));
    }
    if hits.len() > 1 {
        return Err(many());
    }

    // (b) trailing whitespace per line.
    let lines: Vec<&str> = content.split('\n').collect();
    let ol: Vec<&str> = old.split('\n').collect();
    let key = |l: &str| l.trim_end().to_string();
    let okeys: Vec<String> = ol.iter().map(|l| key(l)).collect();
    let mut wins = vec![];
    let mut i = 0;
    while i + ol.len() <= lines.len() {
        if lines[i..i + ol.len()].iter().zip(&okeys).all(|(l, k)| &key(l) == k) {
            wins.push(i);
            i += ol.len();
        } else {
            i += 1;
        }
    }
    if wins.is_empty() {
        return Err(NOT_FOUND.into());
    }
    if wins.len() > 1 && !all {
        return Err(many());
    }
    let mut out: Vec<&str> = vec![];
    let mut at = 0;
    for w in &wins {
        out.extend(&lines[at..*w]);
        out.extend(new.split('\n'));
        at = w + ol.len();
    }
    out.extend(&lines[at..]);
    Ok((out.join("\n"), wins.len()))
}

#[cfg(test)]
mod tests {
    use super::super::testutil::Env;
    use super::*;

    #[test]
    fn exact_unique_many_and_all() {
        assert_eq!(replace("a b c", "b", "X", false).unwrap(), ("a X c".into(), 1));
        assert!(replace("b b", "b", "X", false).unwrap_err().contains("more than once"));
        assert_eq!(replace("b b", "b", "X", true).unwrap(), ("X X".into(), 2));
        assert!(replace("abc", "z", "X", false).unwrap_err().contains("not found"));
    }

    #[test]
    fn typographic_variants() {
        // File has curly quotes and an em dash; model typed ASCII.
        let file = "say \u{201c}hi\u{201d} \u{2014} it\u{2019}s fine";
        let (out, n) = replace(file, "say \"hi\" - it's fine", "ok", false).unwrap();
        assert_eq!((out.as_str(), n), ("ok", 1));
    }

    #[test]
    fn trailing_whitespace_lines() {
        let file = "fn a() {  \n    x();\t\n}\n";
        let (out, _) = replace(file, "fn a() {\n    x();\n}", "fn b() {}", false).unwrap();
        assert_eq!(out, "fn b() {}\n");
    }

    #[test]
    fn crlf_and_bom_preserved() {
        let env = Env::new();
        let p = env.file("w.txt", b"\xef\xbb\xbfone\r\ntwo\r\nthree\r\n");
        Edit.run(&json!({"path":"w.txt","oldString":"one\ntwo","newString":"1\n2"}), &env.ctx()).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"\xef\xbb\xbf1\r\n2\r\nthree\r\n");
    }

    #[test]
    fn explicit_cr_edits_exact_bytes() {
        let env = Env::new();
        let p = env.file("w.txt", b"a\r\nb\r\n");
        // Change line endings: old/new differ only in \r.
        Edit.run(&json!({"path":"w.txt","oldString":"a\r\nb","newString":"a\nb"}), &env.ctx()).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"a\nb\r\n");
    }

    #[test]
    fn rejects_noop_and_declined_leaves_file() {
        let env = Env::new();
        let p = env.file("f.txt", b"keep");
        assert!(Edit.run(&json!({"path":"f.txt","oldString":"a","newString":"a"}), &env.ctx()).is_err());
        *env.perm.reply.lock().unwrap() = Err(ToolError::Declined);
        assert!(Edit.run(&json!({"path":"f.txt","oldString":"keep","newString":"x"}), &env.ctx()).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"keep");
    }
}
