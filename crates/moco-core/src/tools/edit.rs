use super::fsio;
use super::*;

pub struct Edit;

/// One requested replacement.
struct Change {
    old: String,
    new: String,
    all: bool,
}

/// `oldString`/`newString`/`replaceAll` on the call itself, or a list of them under `edits`.
fn changes_from(input: &Value) -> Result<Vec<Change>, ToolError> {
    let one = |v: &Value, at: &str| -> Result<Change, ToolError> {
        let get = |k: &str| v.get(k).and_then(Value::as_str).ok_or_else(|| ToolError::fail(format!("{at}`{k}` is required and must be a string")));
        Ok(Change { old: get("oldString")?.to_string(), new: get("newString")?.to_string(), all: v.get("replaceAll").and_then(Value::as_bool).unwrap_or(false) })
    };
    match input.get("edits").filter(|v| !v.is_null()) {
        None => Ok(vec![one(input, "")?]),
        Some(Value::Array(list)) if !list.is_empty() => {
            if input.get("oldString").is_some() || input.get("newString").is_some() {
                return Err(ToolError::fail("pass either `oldString` and `newString`, or `edits`, not both"));
            }
            list.iter().enumerate().map(|(i, v)| one(v, &format!("edits[{i}]: "))).collect()
        }
        Some(Value::Array(_)) => Err(ToolError::fail("`edits` must not be empty")),
        Some(_) => Err(ToolError::fail("`edits` must be a list of {oldString, newString, replaceAll?}")),
    }
}

impl Tool for Edit {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn description(&self) -> String {
        "Performs exact string replacement in a file. `oldString` must match exactly once unless `replaceAll` is set; \
include enough surrounding context to make it unique. Do not include the `N: ` line-number prefix shown by the read tool. \
`newString` must differ from `oldString`. To make several changes to one file in a single call, pass `edits` instead: a list of \
{oldString, newString, replaceAll?} applied in order, each to the result of the one before. If any of them fails, nothing is written. \
If a call fails, the error says what is closest in the file: correct the call and retry edit.".into()
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "path":{"type":"string"},"oldString":{"type":"string"},"newString":{"type":"string"},
            "replaceAll":{"type":"boolean","description":"Replace every occurrence"},
            "edits":{"type":"array","description":"Several replacements in one file, applied in order and all-or-nothing; use instead of oldString/newString",
                "items":{"type":"object","properties":{"oldString":{"type":"string"},"newString":{"type":"string"},"replaceAll":{"type":"boolean"}},"required":["oldString","newString"]}}},
            "required":["path"]})
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = ctx.resolve(str_arg(input, "path")?);
        let changes = changes_from(input)?;
        let multi = changes.len() > 1;
        let label = |i: usize| if multi { format!("edit {} of {}: ", i + 1, changes.len()) } else { String::new() };
        for (i, c) in changes.iter().enumerate() {
            if c.old.is_empty() {
                return Err(ToolError::fail(format!("{}oldString must not be empty", label(i))));
            }
            if c.old == c.new {
                return Err(ToolError::fail(format!("{}newString must be different from oldString", label(i))));
            }
        }
        // Resolved once: the checks judge this name and the edit uses exactly it, refusing any symlink found in it by
        // then (one swapped in after the check, #165).
        let real = real_path(&path);
        ctx.check_external_file_at(&path, &real)?;
        ctx.permit_path_at("edit", &path, &real, vec!["*".into()])?;
        // An edit shows what the file holds: whether oldString matches and how often, and on a miss the closest
        // text in the file. So it is a `read` too, checked as the read tool checks it: `.env` asks even where edits
        // do not. Ordinary files pass silently (`read *` is allowed in every mode).
        if real.is_file() {
            ctx.permit_path_at("read", &path, &real, vec!["*".into()])?;
        }
        crate::nofollow::between_check_and_use();
        let path = real;
        // From here to the end of formatting no other call may change this file (issue #6).
        let _locks = fsio::lock_files([&path]);
        let bytes = crate::nofollow::read(&path).map_err(|e| super::read::file_error(&path, e))?;
        let (bom, body) = match bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
            Some(rest) => (true, rest),
            None => (false, &bytes[..]),
        };

        let mut replacements = 0;
        let updated: Vec<u8> = match std::str::from_utf8(body) {
            Ok(text) => {
                let mut text = text.to_string();
                for (i, c) in changes.iter().enumerate() {
                    let (next, n) = apply(&text, c).map_err(|e| ToolError::fail(format!("{}{e}", label(i))))?;
                    text = next;
                    replacements += n;
                }
                text.into_bytes()
            }
            Err(_) => {
                // Not text. ASCII strings can still be matched byte for byte.
                let mut data = body.to_vec();
                for (i, c) in changes.iter().enumerate() {
                    if !c.old.is_ascii() || !c.new.is_ascii() {
                        return Err(ToolError::fail(format!("{}the file is not valid UTF-8, so edit can only change it with plain ASCII text in oldString and newString (matched byte for byte); use write to replace the whole file", label(i))));
                    }
                    let (next, n) = replace_bytes(&data, &c.old, &c.new, c.all).map_err(|e| ToolError::fail(format!("{}{e}", label(i))))?;
                    data = next;
                    replacements += n;
                }
                data
            }
        };
        let mut out = Vec::with_capacity(updated.len() + 3);
        if bom {
            out.extend_from_slice(&[0xef, 0xbb, 0xbf]);
        }
        out.extend_from_slice(&updated);
        fsio::write_atomic(&path, &out).map_err(|e| super::read::file_error(&path, e))?;
        let note = ctx.format_file(&path, &out).map(|n| format!("\n{n}")).unwrap_or_default();
        let plural = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
        let what = if multi { format!("{}, {}", plural(changes.len(), "edit"), plural(replacements, "replacement")) } else { plural(replacements, "replacement") };
        Ok(ToolOutput { text: format!("Edit applied: {what} in {}{note}", ctx.display(&path)), metadata: json!({"path": path.to_string_lossy(), "replacements": replacements}) })
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

/// How a replacement attempt ended.
enum Found {
    Done(String, usize),
    /// More than one place matches and `replaceAll` is off: the 1-based line of each.
    Many(Vec<usize>),
    Missing,
}

fn line_at(content: &str, byte: usize) -> usize {
    1 + content.as_bytes()[..byte].iter().filter(|b| **b == b'\n').count()
}

fn many_message(lines: &[usize]) -> String {
    let shown: Vec<String> = lines.iter().take(10).map(|l| l.to_string()).collect();
    let more = if lines.len() > 10 { format!(" and {} more", lines.len() - 10) } else { String::new() };
    format!("oldString matches more than once ({} times, at lines {}{more}). Add surrounding context to make it unique, or set replaceAll.", lines.len(), shown.join(", "))
}

fn not_found_message(content: &str, old: &str) -> String {
    match closest_region(content, old, "edit") {
        Some(hint) => format!("oldString not found in file. It must match the file content exactly (including whitespace).\n{hint}"),
        None => NOT_FOUND.to_string(),
    }
}

/// The read tool puts `N: ` in front of every line. When every line of `s` carries it, `s` without those prefixes.
fn strip_read_prefix(s: &str) -> Option<String> {
    static PREFIX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = PREFIX.get_or_init(|| regex::Regex::new(r"^\d+:( |$)").unwrap());
    let lines: Vec<&str> = s.split('\n').collect();
    let body = if lines.last() == Some(&"") { &lines[..lines.len() - 1] } else { &lines[..] };
    if body.is_empty() || !body.iter().all(|l| re.is_match(l)) {
        return None;
    }
    let mut out = body.iter().map(|l| &l[re.find(l).map_or(0, |m| m.end())..]).collect::<Vec<_>>().join("\n");
    if lines.last() == Some(&"") {
        out.push('\n');
    }
    Some(out)
}

/// One change applied to `content`, with CRLF files and a copied line-number prefix taken care of.
fn apply(content: &str, c: &Change) -> Result<(String, usize), String> {
    // The model writes `\n`. For CRLF files convert, unless it deliberately included `\r`.
    let crlf = content.contains("\r\n") && !c.old.contains('\r');
    let conv = |s: &str| if crlf { to_crlf(s) } else { s.to_string() };
    match try_replace(content, &conv(&c.old), &conv(&c.new), c.all) {
        Found::Done(out, n) => Ok((out, n)),
        Found::Many(lines) => Err(many_message(&lines)),
        Found::Missing => {
            // Models sometimes copy the `N: ` prefix of the read tool into oldString (and newString).
            let Some(old) = strip_read_prefix(&c.old) else { return Err(not_found_message(content, &c.old)) };
            let new = strip_read_prefix(&c.new).unwrap_or_else(|| c.new.clone());
            match try_replace(content, &conv(&old), &conv(&new), c.all) {
                Found::Done(out, n) => Ok((out, n)),
                Found::Many(lines) => Err(many_message(&lines)),
                Found::Missing => Err(not_found_message(content, &old)),
            }
        }
    }
}

/// Exact match, then typographic-variant match, then per-line trailing-whitespace-insensitive match.
pub fn replace(content: &str, old: &str, new: &str, all: bool) -> Result<(String, usize), String> {
    if old.is_empty() {
        return Err("oldString must not be empty".into());
    }
    match try_replace(content, old, new, all) {
        Found::Done(out, n) => Ok((out, n)),
        Found::Many(lines) => Err(many_message(&lines)),
        Found::Missing => Err(not_found_message(content, old)),
    }
}

fn try_replace(content: &str, old: &str, new: &str, all: bool) -> Found {
    let exact: Vec<usize> = content.match_indices(old).map(|(i, _)| line_at(content, i)).collect();
    if exact.len() == 1 || (exact.len() > 1 && all) {
        return Found::Done(content.replace(old, new), exact.len());
    }
    if exact.len() > 1 {
        return Found::Many(exact);
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
        return Found::Done(out, hits.len());
    }
    if hits.len() > 1 {
        return Found::Many(hits.iter().map(|h| 1 + c[..*h].iter().filter(|x| **x == '\n').count()).collect());
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
        return Found::Missing;
    }
    if wins.len() > 1 && !all {
        return Found::Many(wins.iter().map(|w| w + 1).collect());
    }
    let mut out: Vec<&str> = vec![];
    let mut at = 0;
    for w in &wins {
        out.extend(&lines[at..*w]);
        out.extend(new.split('\n'));
        at = w + ol.len();
    }
    out.extend(&lines[at..]);
    Found::Done(out.join("\n"), wins.len())
}

/// Exact byte matching for files that are not valid UTF-8 (ASCII strings only).
fn replace_bytes(data: &[u8], old: &str, new: &str, all: bool) -> Result<(Vec<u8>, usize), String> {
    let (old, new) = (old.as_bytes(), new.as_bytes());
    let mut at = vec![];
    let mut i = 0;
    while i + old.len() <= data.len() {
        if &data[i..i + old.len()] == old {
            at.push(i);
            i += old.len();
        } else {
            i += 1;
        }
    }
    if at.is_empty() {
        return Err("oldString not found in file (the file is not valid UTF-8, so only an exact byte-for-byte match is tried)".into());
    }
    if at.len() > 1 && !all {
        let lines: Vec<usize> = at.iter().map(|p| 1 + data[..*p].iter().filter(|b| **b == b'\n').count()).collect();
        return Err(many_message(&lines));
    }
    let mut out = Vec::with_capacity(data.len());
    let mut from = 0;
    for p in &at {
        out.extend_from_slice(&data[from..*p]);
        out.extend_from_slice(new);
        from = p + old.len();
    }
    out.extend_from_slice(&data[from..]);
    Ok((out, at.len()))
}

/// Windows the hint looks at: files times lines of `oldString` (a limit on the work, not on the file).
const HINT_MAX_WORK: usize = 1_000_000;
const HINT_MIN_SIMILARITY: f64 = 0.6;
const HINT_LINE_CHARS: usize = 200;
const HINT_LINES_SHOWN: usize = 8;

/// The character pairs of a line (trimmed, typographic variants folded), sorted.
fn grams(line: &str) -> Vec<u64> {
    let cs: Vec<char> = line.trim().chars().map(fold).take(HINT_LINE_CHARS).collect();
    let mut v: Vec<u64> = cs.windows(2).map(|w| ((w[0] as u64) << 32) | w[1] as u64).collect();
    if cs.len() == 1 {
        v.push(cs[0] as u64);
    }
    v.sort_unstable();
    v
}

/// Dice coefficient of two sorted gram lists: 1.0 for equal lines, 0.0 for nothing in common.
fn dice(a: &[u64], b: &[u64]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let (mut i, mut j, mut common) = (0, 0, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
        }
    }
    2.0 * common as f64 / (a.len() + b.len()) as f64
}

fn clip(s: &str) -> String {
    let t: String = s.chars().take(HINT_LINE_CHARS).collect();
    if s.chars().count() > HINT_LINE_CHARS { format!("{t}...") } else { t }
}

/// The place in `content` that `old` most resembles, with line numbers and the first line that differs, so the model
/// can correct its call without reading the file again. `None` when nothing is close enough.
pub(crate) fn closest_region(content: &str, old: &str, tool: &str) -> Option<String> {
    let lines: Vec<&str> = content.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    let mut want: Vec<&str> = old.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    while want.len() > 1 && want.last().is_some_and(|l| l.trim().is_empty()) {
        want.pop();
    }
    let k = want.len();
    if lines.len() < k || lines.len().saturating_mul(k) > HINT_MAX_WORK {
        return None;
    }
    let lg: Vec<Vec<u64>> = lines.iter().map(|l| grams(l)).collect();
    let wg: Vec<Vec<u64>> = want.iter().map(|l| grams(l)).collect();
    let (mut best, mut at) = (0.0f64, 0usize);
    for i in 0..=lines.len() - k {
        let s = (0..k).map(|j| dice(&lg[i + j], &wg[j])).sum::<f64>() / k as f64;
        if s > best {
            (best, at) = (s, i);
        }
    }
    if best < HINT_MIN_SIMILARITY {
        return None;
    }
    let shown = k.min(HINT_LINES_SHOWN);
    let region: Vec<String> = (at..at + shown).map(|i| format!("{}: {}", i + 1, clip(lines[i]))).collect();
    let more = if k > shown { format!("\n... ({} more lines)", k - shown) } else { String::new() };
    let first = (0..k).find(|&j| lines[at + j].trim_end() != want[j].trim_end()).unwrap_or(0);
    Some(format!(
        "The closest text in the file is at lines {}-{} ({}% similar):\n{}{more}\nFirst difference, line {}: you wrote `{}` but the file has `{}`. Correct the text from this hint (read with offset and limit shows more) and call {tool} again.",
        at + 1,
        at + k,
        (best * 100.0).round() as u32,
        region.join("\n"),
        at + first + 1,
        clip(want[first]),
        clip(lines[at + first])
    ))
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
    fn edit_reformats_and_says_so() {
        let mut env = Env::new();
        env.formatters = crate::format::tests::upcase_formatters();
        let p = env.file("a.txt", b"hello world");
        let o = Edit.run(&json!({"path":"a.txt","oldString":"world","newString":"there"}), &env.ctx()).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "HELLO THERE");
        assert!(o.text.starts_with("Edit applied: 1 replacement in a.txt\nFormatted with upcase:"), "{}", o.text);
    }

    fn lines(n: usize) -> String {
        (1..=n).map(|i| format!("line number {i}\n")).collect()
    }

    #[test]
    fn a_copied_read_prefix_is_stripped_when_every_line_has_it() {
        let env = Env::new();
        let p = env.file("a.rs", b"fn a() {\n    one();\n}\n");
        // Both strings copied from the read tool's output.
        Edit.run(&json!({"path":"a.rs","oldString":"1: fn a() {\n2:     one();","newString":"1: fn a() {\n2:     two();"}), &env.ctx()).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "fn a() {\n    two();\n}\n");
        // Only oldString carries it.
        Edit.run(&json!({"path":"a.rs","oldString":"2:     two();\n","newString":"    three();\n"}), &env.ctx()).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "fn a() {\n    three();\n}\n");
        // A blank line in the file shows up as `3:` with nothing after it.
        env.file("b.txt", b"x\n\ny\n");
        Edit.run(&json!({"path":"b.txt","oldString":"1: x\n2:\n3: y","newString":"x y"}), &env.ctx()).unwrap();
        assert_eq!(std::fs::read_to_string(env.work().join("b.txt")).unwrap(), "x y\n");
    }

    #[test]
    fn the_prefix_is_left_alone_unless_every_line_has_it_and_the_text_then_matches() {
        let env = Env::new();
        // The text really contains numbers and colons: an exact match is never second-guessed.
        let p = env.file("n.txt", b"1: first\n2: second\n");
        Edit.run(&json!({"path":"n.txt","oldString":"1: first","newString":"one"}), &env.ctx()).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one\n2: second\n");
        // One line without the prefix: not stripped, so no match.
        env.file("m.txt", b"a\nb\n");
        assert!(Edit.run(&json!({"path":"m.txt","oldString":"1: a\nb","newString":"x"}), &env.ctx()).is_err());
        // Stripped, but the text is not in the file either.
        let e = Edit.run(&json!({"path":"m.txt","oldString":"1: a\n2: z","newString":"x"}), &env.ctx()).unwrap_err();
        assert!(format!("{e:?}").contains("not found"), "{e:?}");
        assert_eq!(std::fs::read_to_string(env.work().join("m.txt")).unwrap(), "a\nb\n");
    }

    fn message(e: ToolError) -> String {
        match e {
            ToolError::Failure(m) => m,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn not_found_names_the_closest_text_and_the_first_difference() {
        let env = Env::new();
        let body = format!("{}    let total = items.len();\n    println!(\"{{total}}\");\n{}", lines(40), lines(40));
        env.file("s.rs", body.as_bytes());
        // The model remembered `count` instead of `total`, and the indentation wrong.
        let e = message(Edit.run(&json!({"path":"s.rs","oldString":"    let count = items.len();\n    println!(\"{count}\");","newString":"x"}), &env.ctx()).unwrap_err());
        assert!(e.contains("not found") && e.contains("lines 41-42"), "{e}");
        assert!(e.contains("41:     let total = items.len();") && e.contains("42:"), "the region is quoted with line numbers: {e}");
        assert!(e.contains("First difference, line 41: you wrote `    let count = items.len();` but the file has `    let total = items.len();`"), "{e}");
        // Nothing resembles it: the plain message, no invented location.
        let e = message(Edit.run(&json!({"path":"s.rs","oldString":"zzzz qqqq xxxx","newString":"x"}), &env.ctx()).unwrap_err());
        assert!(e.contains("not found") && !e.contains("closest"), "{e}");
        // A single wrong character in one line.
        env.file("one.txt", b"alpha\nbeta gamma delta\nepsilon\n");
        let e = message(Edit.run(&json!({"path":"one.txt","oldString":"beta gamma delte","newString":"x"}), &env.ctx()).unwrap_err());
        assert!(e.contains("line 2") && e.contains("beta gamma delta"), "{e}");
    }

    #[test]
    fn a_leading_indent_difference_is_reported_as_the_first_difference() {
        let env = Env::new();
        env.file("i.py", b"def f():\n    if x:\n        return 1\n");
        let e = message(Edit.run(&json!({"path":"i.py","oldString":"if x:\n    return 1","newString":"y"}), &env.ctx()).unwrap_err());
        assert!(e.contains("First difference, line 2") && e.contains("you wrote `if x:`") && e.contains("the file has `    if x:`"), "{e}");
    }

    #[test]
    fn several_matches_list_their_line_numbers() {
        let env = Env::new();
        env.file("m.txt", b"foo\nbar\nfoo\nbaz\nfoo\n");
        let e = message(Edit.run(&json!({"path":"m.txt","oldString":"foo","newString":"x"}), &env.ctx()).unwrap_err());
        assert!(e.contains("matches more than once (3 times, at lines 1, 3, 5)"), "{e}");
        // Many matches: the first ten.
        env.file("many.txt", "x\n".repeat(15).as_bytes());
        let e = message(Edit.run(&json!({"path":"many.txt","oldString":"x","newString":"y"}), &env.ctx()).unwrap_err());
        assert!(e.contains("15 times, at lines 1, 2, 3, 4, 5, 6, 7, 8, 9, 10 and 5 more"), "{e}");
        // The typographic and trailing-whitespace paths count lines too.
        assert!(replace("say \u{201c}a\u{201d}\nx\nsay \u{201c}a\u{201d}\n", "say \"a\"", "y", false).unwrap_err().contains("lines 1, 3"));
        assert!(replace("a  \nb  \na  \nb  \n", "a\nb", "y", false).unwrap_err().contains("lines 1, 3"));
    }

    #[test]
    fn edits_apply_in_order_to_the_result_of_the_one_before() {
        let env = Env::new();
        let p = env.file("e.txt", b"alpha beta\nbeta beta\n");
        let o = Edit.run(&json!({"path":"e.txt","edits":[
            {"oldString":"alpha","newString":"ALPHA"},
            {"oldString":"ALPHA beta","newString":"first"},
            {"oldString":"beta","newString":"B","replaceAll":true}]}), &env.ctx()).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "first\nB B\n");
        assert!(o.text.starts_with("Edit applied: 3 edits, 4 replacements in e.txt"), "{}", o.text);
        assert_eq!(o.metadata["replacements"], 4);
    }

    #[test]
    fn edits_are_all_or_nothing_and_name_the_one_that_failed() {
        let env = Env::new();
        let p = env.file("e.txt", b"one two three\n");
        let e = message(Edit.run(&json!({"path":"e.txt","edits":[
            {"oldString":"one","newString":"1"},
            {"oldString":"two","newString":"2"},
            {"oldString":"nine","newString":"9"}]}), &env.ctx()).unwrap_err());
        assert!(e.starts_with("edit 3 of 3: oldString not found"), "{e}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one two three\n", "nothing was written");
        // A later edit that no longer matches because an earlier one changed the text.
        let e = message(Edit.run(&json!({"path":"e.txt","edits":[{"oldString":"one","newString":"1"},{"oldString":"one two","newString":"x"}]}), &env.ctx()).unwrap_err());
        assert!(e.starts_with("edit 2 of 2:"), "{e}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one two three\n");
        // Bad shapes.
        for bad in [json!({"path":"e.txt","edits":[]}), json!({"path":"e.txt","edits":"x"}), json!({"path":"e.txt","oldString":"a","newString":"b","edits":[{"oldString":"a","newString":"c"}]}), json!({"path":"e.txt","edits":[{"oldString":"a"}]}), json!({"path":"e.txt","edits":[{"oldString":"a","newString":"a"}]})] {
            assert!(Edit.run(&bad, &env.ctx()).is_err(), "{bad}");
        }
        // `edits: null` next to the plain form (models emit nulls) is the plain form.
        Edit.run(&json!({"path":"e.txt","oldString":"one","newString":"1","edits":null}), &env.ctx()).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "1 two three\n");
    }

    #[test]
    fn a_declined_multi_edit_changes_nothing() {
        let env = Env::new();
        let p = env.file("d.txt", b"keep");
        *env.perm.reply.lock().unwrap() = Err(ToolError::Declined);
        assert!(Edit.run(&json!({"path":"d.txt","edits":[{"oldString":"keep","newString":"x"}]}), &env.ctx()).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"keep");
    }

    #[test]
    fn files_that_are_not_utf8_take_ascii_edits_byte_for_byte() {
        let env = Env::new();
        let p = env.file("latin.txt", b"caf\xe9 au lait\nname=old\n");
        let o = Edit.run(&json!({"path":"latin.txt","oldString":"name=old","newString":"name=new"}), &env.ctx()).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"caf\xe9 au lait\nname=new\n", "the bytes around the change are untouched");
        assert!(o.text.contains("1 replacement"));
        let e = message(Edit.run(&json!({"path":"latin.txt","oldString":"caf\u{e9}","newString":"cafe"}), &env.ctx()).unwrap_err());
        assert!(e.contains("not valid UTF-8") && e.contains("ASCII") && e.contains("write"), "{e}");
        let e = message(Edit.run(&json!({"path":"latin.txt","oldString":"zzz","newString":"y"}), &env.ctx()).unwrap_err());
        assert!(e.contains("not found") && e.contains("byte-for-byte"), "{e}");
        env.file("two.bin", b"\xff a\n\xfe a\n");
        let e = message(Edit.run(&json!({"path":"two.bin","oldString":"a","newString":"b"}), &env.ctx()).unwrap_err());
        assert!(e.contains("at lines 1, 2"), "{e}");
        Edit.run(&json!({"path":"two.bin","oldString":"a","newString":"b","replaceAll":true}), &env.ctx()).unwrap();
        assert_eq!(std::fs::read(env.work().join("two.bin")).unwrap(), b"\xff b\n\xfe b\n");
    }

    #[test]
    fn an_edit_keeps_the_mode_and_leaves_no_temporary_file() {
        use std::os::unix::fs::PermissionsExt;
        let env = Env::new();
        let p = env.file("run.sh", b"#!/bin/sh\necho old\n");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        Edit.run(&json!({"path":"run.sh","oldString":"old","newString":"new"}), &env.ctx()).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o755);
        let names: Vec<String> = std::fs::read_dir(env.work()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["run.sh"], "{names:?}");
    }

    #[test]
    fn what_the_formatter_changed_comes_back_with_line_numbers() {
        let mut env = Env::new();
        env.formatters = crate::format::tests::upcase_formatters();
        env.file("a.txt", b"keep\nhello world\n");
        let o = Edit.run(&json!({"path":"a.txt","oldString":"world","newString":"there"}), &env.ctx()).unwrap();
        assert!(o.text.contains("-1: keep") && o.text.contains("+1: KEEP") && o.text.contains("+2: HELLO THERE"), "{}", o.text);
        assert!(o.text.contains("no need to read the file again"), "{}", o.text);
    }

    /// Issue #6: several `edit` calls on one file in a step run on separate threads; every one of them must land.
    #[test]
    fn parallel_edits_of_one_file_all_land() {
        let env = Env::new();
        let n = 12;
        let body: String = (0..n).map(|i| format!("slot{i}=old\n")).collect();
        let p = env.file("p.txt", body.as_bytes());
        std::thread::scope(|s| {
            for i in 0..n {
                let env = &env;
                s.spawn(move || {
                    Edit.run(&json!({"path":"p.txt","oldString":format!("slot{i}=old"),"newString":format!("slot{i}=new")}), &env.ctx()).unwrap();
                });
            }
        });
        let after = std::fs::read_to_string(&p).unwrap();
        assert_eq!(after.matches("=new").count(), n, "no update was lost:\n{after}");
        assert!(!after.contains("=old"), "{after}");
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

    /// Issue #119: a guessed oldString must not get the real line of a `.env` file back in the error, nor tell by
    /// succeeding or failing what the file holds, unless reading it is allowed.
    #[test]
    fn editing_a_file_the_read_rules_protect_asks_for_read_first() {
        use super::super::grep::tests::Rules;
        use crate::permission::{Effect, PermissionMode, Rule};
        let env = Env::new();
        let p = env.file(".env", b"DB_USER=app\nDB_PASSWORD=hunter2\n");
        let edit = |perm: &Rules, old: &str| Edit.run(&json!({"path":".env","oldString":old,"newString":"DB_PASSWORD=x"}), &ToolCtx { permitter: perm, ..env.ctx() });
        let perm = Rules::auto();
        assert_eq!(edit(&perm, "DB_PASSWORD=hunted").unwrap_err(), ToolError::Declined);
        assert_eq!(edit(&perm, "DB_PASSWORD=hunter2").unwrap_err(), ToolError::Declined);
        assert_eq!(perm.prompts(), ["read .env", "read .env"]);
        assert_eq!(std::fs::read(&p).unwrap(), b"DB_USER=app\nDB_PASSWORD=hunter2\n");
        // Manual mode, once the user has approved the edit itself.
        let perm = Rules::new(PermissionMode::Manual, vec![Rule::new("edit", "*", Effect::Allow)]);
        assert_eq!(edit(&perm, "DB_PASSWORD=hunted").unwrap_err(), ToolError::Declined);
        assert_eq!(perm.prompts(), ["read .env"]);
        // Reading it allowed by the user's rules: the usual hint.
        let perm = Rules::new(PermissionMode::Auto, vec![Rule::new("read", ".env", Effect::Allow)]);
        let e = message(edit(&perm, "DB_PASSWORD=hunted").unwrap_err());
        assert!(e.contains("closest text") && e.contains("hunter2"), "{e}");

        // An ordinary file in auto mode: no prompt, and the hint quotes the file as before.
        env.file("conf.txt", b"name = moco\n");
        let perm = Rules::auto();
        let e = message(Edit.run(&json!({"path":"conf.txt","oldString":"name = mocca","newString":"x"}), &ToolCtx { permitter: &perm, ..env.ctx() }).unwrap_err());
        assert!(e.contains("name = moco"), "{e}");
        Edit.run(&json!({"path":"conf.txt","oldString":"name = moco","newString":"name = x"}), &ToolCtx { permitter: &perm, ..env.ctx() }).unwrap();
        assert!(perm.prompts().is_empty(), "{:?}", perm.prompts());
    }
}
