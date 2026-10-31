//! What a file-changing tool call looks like in the transcript, built from the call's input.
use serde_json::Value;
use similar::TextDiff;

const MAX_DETAIL_LINES: usize = 200;

/// Diff text for `edit`, `write` and `patch` calls (unified-diff style, `+`/`-`/`@@` lines); `None` for other tools.
pub fn tool_detail(name: &str, input: &Value) -> Option<String> {
    let text = match name {
        "edit" => {
            let (old, new) = (input["oldString"].as_str()?, input["newString"].as_str()?);
            let diff = TextDiff::from_lines(old, new);
            let full = diff.unified_diff().context_radius(2).to_string();
            // Drop the `---`/`+++` header: the tool row already names the file.
            full.lines().filter(|l| !l.starts_with("--- ") && !l.starts_with("+++ ")).collect::<Vec<_>>().join("\n")
        }
        "write" => {
            let content = input["content"].as_str()?;
            let lines: Vec<&str> = content.lines().collect();
            let mut out: Vec<String> = lines.iter().take(MAX_DETAIL_LINES).map(|l| format!("+{l}")).collect();
            if lines.len() > MAX_DETAIL_LINES {
                out.push(format!("... {} more lines", lines.len() - MAX_DETAIL_LINES));
            }
            out.join("\n")
        }
        "patch" => {
            let patch = input["patchText"].as_str()?;
            patch.lines().filter(|l| !l.starts_with("*** Begin") && !l.starts_with("*** End")).take(MAX_DETAIL_LINES).collect::<Vec<_>>().join("\n")
        }
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

/// First `max` lines of a tool's output, for the full-detail view.
pub fn output_head(text: &str, max: usize) -> String {
    let mut lines: Vec<&str> = text.lines().take(max + 1).collect();
    let more = lines.len() > max;
    lines.truncate(max);
    let mut s = lines.join("\n");
    if more {
        s.push_str("\n...");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edit_shows_only_the_changed_lines_with_context() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "a\nb\nc\nD\ne\nf\ng\nh\n";
        let d = tool_detail("edit", &json!({"path": "x", "oldString": old, "newString": new})).unwrap();
        assert!(d.starts_with("@@ ") && d.contains("-d\n+D") && d.contains(" b\n c\n") && !d.contains("--- ") && !d.contains("+++ "), "{d}");
        assert!(!d.contains(" h"), "far context is left out: {d}");
    }

    #[test]
    fn write_and_patch_and_other_tools() {
        let long = (0..300).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let d = tool_detail("write", &json!({"path": "x", "content": long})).unwrap();
        assert!(d.starts_with("+line 0\n") && d.ends_with("... 100 more lines"), "{}", &d[d.len() - 40..]);
        let p = tool_detail("patch", &json!({"patchText": "*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\n*** End Patch"})).unwrap();
        assert_eq!(p, "*** Update File: a.rs\n@@\n-x\n+y");
        assert_eq!(tool_detail("read", &json!({"path": "x"})), None);
        assert_eq!(tool_detail("edit", &json!({"path": "x"})), None); // incomplete input while streaming
        assert_eq!(tool_detail("write", &json!({"path": "x", "content": ""})), None);
    }

    #[test]
    fn output_head_cuts_long_output() {
        assert_eq!(output_head("a\nb\nc", 5), "a\nb\nc");
        assert_eq!(output_head("a\nb\nc\nd", 2), "a\nb\n...");
    }
}
