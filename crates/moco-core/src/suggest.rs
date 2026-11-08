//! The suggested next message shown as ghost text in the TUI composer after a turn (issue #66). A small model reads
//! the last prompt, the reply and any open to-dos and proposes one short message the user might send next. The user
//! takes it with Tab or ignores it by typing; it is never sent on its own.

pub const SYSTEM: &str = "You suggest the next message a user might send to their coding assistant.

You are shown the user's last message, the assistant's reply and any unfinished to-do items. Write the one message the user is most likely to send next.

Rules:
- Output exactly one line: the message, nothing else. No quotes, no label, no explanation.
- Write it as the user, as a short instruction to the assistant: at most 12 words. Examples: \"Run the tests\", \"Commit the changes\", \"Fix the failing lint in src/main.rs\", \"Continue with the next to-do\".
- It must follow from the conversation: the obvious next step, an open to-do, or what the assistant offered to do next.
- Prefer safe, reversible steps. Never suggest deleting data, force-pushing, or anything the conversation does not point to.
- If the reply asks the user a question only they can answer, or there is no clear next step, output exactly: NONE
- Use the language the user writes in.";

/// The message the suggestion is written from. Long texts are cut, the reply from its end (where next steps are).
pub fn request_text(prompt: &str, reply: &str, open_todos: &[String]) -> String {
    let head = |s: &str, n: usize| -> String {
        let s = s.trim();
        if s.chars().count() > n { format!("{}...", s.chars().take(n).collect::<String>()) } else { s.to_string() }
    };
    let tail = |s: &str, n: usize| -> String {
        let s = s.trim();
        let len = s.chars().count();
        if len > n { format!("...{}", s.chars().skip(len - n).collect::<String>()) } else { s.to_string() }
    };
    let mut out = format!("<user message>\n{}\n</user message>\n\n<assistant reply>\n{}\n</assistant reply>", head(prompt, 1500), tail(reply, 3000));
    if !open_todos.is_empty() {
        let items: Vec<String> = open_todos.iter().take(10).map(|t| format!("- {}", head(t, 200))).collect();
        out.push_str(&format!("\n\n<open to-dos>\n{}\n</open to-dos>", items.join("\n")));
    }
    out.push_str("\n\nWrite the user's next message, or NONE.");
    out
}

/// The suggestion from a model reply, or `None` when there is none or it is unusable. One line of printable text:
/// it is drawn in the terminal and may become the user's message.
pub fn clean(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let strip = |s: &str| s.trim().trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '*' | '\u{201c}' | '\u{201d}')).trim().to_string();
    let mut t = strip(line.trim_start_matches(['-', '>', '#']));
    for label in ["suggestion:", "next message:", "next:", "user:"] {
        if t.len() >= label.len() && t.is_char_boundary(label.len()) && t[..label.len()].eq_ignore_ascii_case(label) {
            t = strip(&t[label.len()..]);
        }
    }
    let t: String = t.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let bare = t.trim_end_matches(['.', '!']);
    if bare.is_empty() || bare.eq_ignore_ascii_case("none") || t.chars().count() > 120 {
        return None;
    }
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_become_one_short_line_or_nothing() {
        for (raw, want) in [
            ("Run the tests", Some("Run the tests")),
            ("\"Commit the changes\"\n", Some("Commit the changes")),
            ("\n  Suggestion: Fix the lint in src/main.rs  \n(because it failed)", Some("Fix the lint in src/main.rs")),
            ("- `Continue with the next to-do`", Some("Continue with the next to-do")),
            ("Next message: Push the branch", Some("Push the branch")),
            ("Run\tthe\u{1b}[2J tests", Some("Run the [2J tests")),
            ("NONE", None),
            ("none.", None),
            ("", None),
            ("   \n  ", None),
            ("\"\"", None),
        ] {
            assert_eq!(clean(raw).as_deref(), want, "{raw:?}");
        }
        assert_eq!(clean(&"word ".repeat(40)), None, "too long to be a suggestion");
    }

    #[test]
    fn the_request_keeps_the_end_of_a_long_reply_and_lists_open_todos() {
        let reply = format!("{}NEXT: run the migration", "x".repeat(5000));
        let text = request_text("add a migration", &reply, &["Write the migration".into(), "Update the docs".into()]);
        assert!(text.contains("NEXT: run the migration"), "the end of the reply is kept");
        assert!(text.contains("<open to-dos>\n- Write the migration\n- Update the docs\n</open to-dos>"));
        assert!(text.chars().count() < 5000);
        assert!(!request_text("hi", "hello", &[]).contains("<open to-dos>"));
    }
}
