//! Export a stored conversation as Markdown, JSON or JSON Lines (`moco sessions export`, `/export`).
//!
//! The export only reads, so it is safe while another process runs the session (the database is in WAL mode).
//! **Exports are not redacted**: tool inputs and outputs, and anything the user typed, appear as stored, including
//! any API key that was pasted into the conversation. Look through an export before you share it.
use crate::store::{Result, Store};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Markdown,
    Json,
    Jsonl,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub format: Format,
    /// Also export subagent sessions (inline in Markdown, nested objects in JSON).
    pub children: bool,
    /// Include messages hidden by a pending undo.
    pub include_hidden: bool,
    /// Tool output longer than this many lines is cut in Markdown.
    pub max_output_lines: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options { format: Format::Markdown, children: false, include_hidden: false, max_output_lines: 40 }
    }
}

/// Render the session `session_id` (an exact id).
pub fn export(store: &Store, session_id: &str, opts: &Options) -> Result<String> {
    match opts.format {
        Format::Markdown => {
            let mut out = String::new();
            markdown(store, session_id, opts, 1, &mut out)?;
            Ok(out)
        }
        Format::Json => Ok(serde_json::to_string_pretty(&tree(store, session_id, opts)?)? + "\n"),
        Format::Jsonl => {
            // One line per message; the first line is the session metadata. Children follow, each starting with its own metadata line.
            let mut out = String::new();
            jsonl(store, session_id, opts, &mut out)?;
            Ok(out)
        }
    }
}

/// The messages of a session as stored, minus the ones a pending undo hides (unless asked for).
fn rows(store: &Store, sid: &str, opts: &Options) -> Result<Vec<Value>> {
    let mut rows = store.message_rows(sid)?;
    if !opts.include_hidden {
        if let Some(h) = store.revert(sid)?.and_then(|r| r["messageId"].as_str().map(String::from)) {
            rows.retain(|m| m["id"].as_str().is_some_and(|id| id < h.as_str()));
        }
    }
    Ok(rows)
}

fn tree(store: &Store, sid: &str, opts: &Options) -> Result<Value> {
    let mut meta = store.session_meta(sid)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    meta["messages"] = Value::Array(rows(store, sid, opts)?);
    if opts.children {
        let mut kids = vec![];
        for c in store.child_session_ids(sid)? {
            kids.push(tree(store, &c, opts)?);
        }
        meta["children"] = Value::Array(kids);
    }
    Ok(meta)
}

fn jsonl(store: &Store, sid: &str, opts: &Options, out: &mut String) -> Result<()> {
    let meta = store.session_meta(sid)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    out.push_str(&json!({"session": meta}).to_string());
    out.push('\n');
    for m in rows(store, sid, opts)? {
        out.push_str(&m.to_string());
        out.push('\n');
    }
    if opts.children {
        for c in store.child_session_ids(sid)? {
            jsonl(store, &c, opts, out)?;
        }
    }
    Ok(())
}

/// A fence longer than any run of backticks in `text` (at least three), so the content cannot close it.
fn fence_for(text: &str) -> String {
    let (mut longest, mut run) = (0, 0);
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat(longest.max(2) + 1)
}

fn fenced(out: &mut String, lang: &str, text: &str) {
    let f = fence_for(text);
    out.push_str(&format!("{f}{lang}\n{}\n{f}\n\n", text.trim_end_matches('\n')));
}

/// Keep the first `max` lines of a tool output. The "full output saved to" notice of a truncated output survives the cut.
fn cut(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max {
        return text.to_string();
    }
    let notice = lines.iter().rev().take(3).find(|l| l.starts_with("[showing lines") && l.contains("full output saved to")).copied();
    let mut kept = lines[..max].join("\n");
    kept.push_str(&format!("\n[... {} more lines not shown in this export]", lines.len() - max));
    if let Some(n) = notice.filter(|n| !kept.contains(*n)) {
        kept.push('\n');
        kept.push_str(n);
    }
    kept
}

/// `2026-11-02 15:52:07 UTC` from milliseconds since the epoch.
pub fn utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Civil date from a day count (Howard Hinnant's algorithm).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn size(bytes: u64) -> String {
    if bytes >= 1 << 20 {
        format!("{:.1} MiB", bytes as f64 / f64::from(1 << 20))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn media_line(m: &Value) -> String {
    let kind = if m["mediaType"].as_str().is_some_and(|t| t.starts_with("image/")) { "image" } else { "file" };
    format!("[{kind}: {}, {}]", m["name"].as_str().unwrap_or("unnamed"), size(m["size"].as_u64().unwrap_or(0)))
}

fn markdown(store: &Store, sid: &str, opts: &Options, level: usize, out: &mut String) -> Result<()> {
    let meta = store.session_meta(sid)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    let h = "#".repeat(level.min(5));
    let s = |k: &str| meta[k].as_str().filter(|v| !v.is_empty());
    out.push_str(&format!("{h} {}\n\n", s("title").unwrap_or("(untitled session)").replace('\n', " ")));
    let (cost, usage) = store.usage_totals(sid)?;
    let mut head = vec![format!("- **Session:** `{sid}`")];
    if let Some(p) = s("parent_id") {
        head.push(format!("- **Subagent of:** `{p}`"));
    }
    head.push(format!("- **Directory:** `{}`", meta["directory"].as_str().unwrap_or("")));
    head.push(format!("- **Agent:** {}", meta["agent"].as_str().unwrap_or("")));
    if let Some(m) = s("model") {
        head.push(format!("- **Model:** {m}"));
    }
    head.push(format!("- **Created:** {}", utc(meta["time_created"].as_i64().unwrap_or(0))));
    head.push(format!("- **Updated:** {}", utc(meta["time_updated"].as_i64().unwrap_or(0))));
    head.push(format!("- **Cost:** ${cost:.4}"));
    head.push(format!("- **Tokens:** {} input, {} output, {} cache read, {} cache write", usage.input, usage.output, usage.cache_read, usage.cache_write));
    if let Some(f) = s("forked_from") {
        head.push(format!("- **Forked from:** `{f}`"));
    }
    out.push_str(&head.join("\n"));
    out.push_str("\n\n");

    let sub = format!("{h}#");
    for m in rows(store, sid, opts)? {
        let data = &m["data"];
        let text = data["text"].as_str().unwrap_or("");
        match m["type"].as_str().unwrap_or("") {
            "user" => {
                out.push_str(&format!("{sub} User\n\n"));
                if !text.is_empty() {
                    out.push_str(&format!("{}\n\n", text.trim_end()));
                }
                let men = &data["mentions"];
                let mut listed = vec![];
                for f in men["files"].as_array().into_iter().flatten() {
                    if f["kind"] == "media" {
                        listed.push(format!("- {}", media_line(f)));
                    } else {
                        listed.push(format!("- `{}`{}", f["path"].as_str().unwrap_or(""), if f["truncated"] == true { " (truncated)" } else { "" }));
                    }
                }
                for key in ["skills", "agents"] {
                    for v in men[key].as_array().into_iter().flatten() {
                        listed.push(format!("- {key} `{}`", v["name"].as_str().or(v.as_str()).unwrap_or("")));
                    }
                }
                if !listed.is_empty() {
                    out.push_str(&format!("Mentions:\n\n{}\n\n", listed.join("\n")));
                }
            }
            "synthetic" => {
                let what = data["description"].as_str().unwrap_or("background result");
                out.push_str(&format!("{sub} Synthetic message ({what})\n\n"));
                fenced(out, "", text);
            }
            "compaction" => {
                out.push_str(&format!("{sub} Compaction\n\n<details>\n<summary>Earlier messages were summarized</summary>\n\n"));
                out.push_str(&format!("{}\n\n</details>\n\n", data["summary"].as_str().unwrap_or("").trim_end()));
            }
            "assistant" => {
                for p in data["parts"].as_array().into_iter().flatten() {
                    match p["type"].as_str() {
                        Some("text") => {
                            if let Some(t) = p["text"].as_str().filter(|t| !t.trim().is_empty()) {
                                out.push_str(&format!("{sub} Assistant\n\n{}\n\n", t.trim_end()));
                            }
                        }
                        Some("tool") => tool(store, p, opts, level, &sub, out)?,
                        // Reasoning and provider-private parts are left out.
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn tool(store: &Store, p: &Value, opts: &Options, level: usize, sub: &str, out: &mut String) -> Result<()> {
    let name = p["name"].as_str().unwrap_or("");
    let st = &p["state"];
    let status = st["status"].as_str().unwrap_or("interrupted");
    out.push_str(&format!("{sub} Tool: `{name}` ({status})\n\n"));
    fenced(out, "json", &serde_json::to_string_pretty(&p["input"]).unwrap_or_default());
    match status {
        "completed" => {
            fenced(out, "", &cut(st["output"].as_str().unwrap_or(""), opts.max_output_lines));
            for m in st["media"].as_array().into_iter().flatten() {
                out.push_str(&format!("{}\n\n", media_line(m)));
            }
        }
        "error" => {
            out.push_str("Error:\n\n");
            fenced(out, "", &cut(st["error"].as_str().unwrap_or("error"), opts.max_output_lines));
        }
        _ => out.push_str("(no result: the call was interrupted)\n\n"),
    }
    if opts.children && name == "subagent" {
        if let Some(child) = st["metadata"]["sessionID"].as_str() {
            if store.session_meta(child)?.is_some() {
                markdown(store, child, opts, level + 2, out)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session with every message kind, plus a subagent child.
    fn fixture() -> (Store, String, String) {
        let s = Store::open_memory().unwrap();
        let ses = s.create_session("/work/proj", "build").unwrap();
        s.set_title(&ses.id, "Fix the bug").unwrap();
        let child = s.create_child_session(&ses, "explore").unwrap();
        s.set_title(&child.id, "look around").unwrap();
        s.add_message(&child.id, "user", &json!({"text": "find it"})).unwrap();
        s.add_message(&child.id, "assistant", &json!({"parts": [{"type": "text", "text": "found it"}]})).unwrap();
        s.add_message(&ses.id, "user", &json!({"text": "fix @a.txt", "mentions": {"files": [{"path": "a.txt", "kind": "file", "content": "x", "truncated": false}]}})).unwrap();
        s.add_message(
            &ses.id,
            "assistant",
            &json!({"parts": [
                {"type": "native", "provider": "anthropic", "data": {}},
                {"type": "text", "text": "Looking."},
                {"type": "tool", "id": "t1", "name": "read", "input": {"path": "a.txt"}, "state": {"status": "completed", "output": "a ``` b"}},
                {"type": "tool", "id": "t2", "name": "read", "input": {"path": "b.txt"}, "state": {"status": "error", "error": "no such file"}},
                {"type": "tool", "id": "t3", "name": "subagent", "input": {"prompt": "find it"}, "state": {"status": "completed", "output": "sessionID: x\n\nfound it", "metadata": {"sessionID": child.id}}}]}),
        )
        .unwrap();
        s.add_message(&ses.id, "synthetic", &json!({"text": "job done", "description": "background job"})).unwrap();
        s.add_message(&ses.id, "compaction", &json!({"summary": "SUMMARY", "keepFrom": "m"})).unwrap();
        (s, ses.id, child.id)
    }

    /// Replace the volatile ids and times so the output can be compared with a fixed text.
    fn stable(text: &str, ids: &[&str]) -> String {
        let mut t = text.to_string();
        for (i, id) in ids.iter().enumerate() {
            t = t.replace(id, &format!("ID{i}"));
        }
        t.lines().filter(|l| !l.starts_with("- **Created:**") && !l.starts_with("- **Updated:**")).collect::<Vec<_>>().join("\n") + "\n"
    }

    #[test]
    fn markdown_transcript() {
        let (s, id, child) = fixture();
        let md = export(&s, &id, &Options { children: true, ..Options::default() }).unwrap();
        let expected = "\
# Fix the bug

- **Session:** `ID0`
- **Directory:** `/work/proj`
- **Agent:** build
- **Cost:** $0.0000
- **Tokens:** 0 input, 0 output, 0 cache read, 0 cache write

## User

fix @a.txt

Mentions:

- `a.txt`

## Assistant

Looking.

## Tool: `read` (completed)

```json
{
  \"path\": \"a.txt\"
}
```

````
a ``` b
````

## Tool: `read` (error)

```json
{
  \"path\": \"b.txt\"
}
```

Error:

```
no such file
```

## Tool: `subagent` (completed)

```json
{
  \"prompt\": \"find it\"
}
```

```
sessionID: x

found it
```

### look around

- **Session:** `ID1`
- **Subagent of:** `ID0`
- **Directory:** `/work/proj`
- **Agent:** explore
- **Cost:** $0.0000
- **Tokens:** 0 input, 0 output, 0 cache read, 0 cache write

#### User

find it

#### Assistant

found it

## Synthetic message (background job)

```
job done
```

## Compaction

<details>
<summary>Earlier messages were summarized</summary>

SUMMARY

</details>

";
        assert_eq!(stable(&md, &[&id, &child]), expected);
        assert!(!export(&s, &id, &Options::default()).unwrap().contains("look around"), "children only with --children");
    }

    #[test]
    fn json_matches_the_message_rows_and_nests_children() {
        let (s, id, child) = fixture();
        let v: Value = serde_json::from_str(&export(&s, &id, &Options { format: Format::Json, children: true, ..Options::default() }).unwrap()).unwrap();
        let stored: Vec<Value> = s.message_rows(&id).unwrap();
        assert_eq!(v["messages"], Value::Array(stored.clone()));
        for (m, row) in stored.iter().zip(s.messages(&id).unwrap()) {
            assert_eq!((m["id"].as_str(), m["type"].as_str(), &m["data"]), (Some(row.id.as_str()), Some(row.kind.as_str()), &row.data));
            assert!(m["time_created"].as_i64().is_some());
        }
        assert_eq!(v["children"][0]["id"], child.as_str());
        assert_eq!(v["children"][0]["messages"].as_array().unwrap().len(), 2);
        assert!(v.get("children").is_some() && serde_json::from_str::<Value>(&export(&s, &id, &Options { format: Format::Json, ..Options::default() }).unwrap()).unwrap().get("children").is_none());

        let lines = export(&s, &id, &Options { format: Format::Jsonl, ..Options::default() }).unwrap();
        let parsed: Vec<Value> = lines.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(parsed.len(), 1 + stored.len());
        assert_eq!(parsed[1..], stored[..]);
    }

    #[test]
    fn undone_messages_are_hidden_unless_asked_for() {
        let (s, id, _) = fixture();
        let all = s.message_rows(&id).unwrap();
        let hidden_from = all[1]["id"].as_str().unwrap().to_string();
        s.set_revert(&id, Some(&json!({"messageId": hidden_from, "redo": null}))).unwrap();
        let count = |include_hidden| {
            let v: Value = serde_json::from_str(&export(&s, &id, &Options { format: Format::Json, include_hidden, ..Options::default() }).unwrap()).unwrap();
            v["messages"].as_array().unwrap().len()
        };
        assert_eq!((count(false), count(true)), (1, all.len()));
    }

    #[test]
    fn long_output_is_cut_but_keeps_the_spill_notice() {
        let text = (1..=10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n") + "\n\n[showing lines 1-10 of 99; full output saved to /tmp/x]";
        let c = cut(&text, 3);
        assert!(c.starts_with("line 1\nline 2\nline 3\n[... ") && c.ends_with("full output saved to /tmp/x]"));
        assert_eq!(fence_for("a ```` b"), "`````");
        assert_eq!(utc(1_793_634_727_000), "2026-11-02 15:52:07 UTC");
    }
}
