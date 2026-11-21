//! Conversation titles (the contract is in this file). The prompt is moco's own, written to the contract
//! in the spec: one line, 3-5 words, no quotes or trailing period or "Title:" label, sentence case, identifiers
//! preserved, the language of the message, naming the message rather than answering it, greetings -> "Greeting".

pub const SYSTEM: &str = "You write short titles for coding-assistant conversations.

Rules:
- Output exactly one line: the title, nothing else.
- 3 to 5 words, sentence case, no quotes, no trailing period, no \"Title:\" label.
- Name the specific subject of the message. Do not answer it, do not address the user, do not add commentary.
- Keep identifiers (file names, error codes, function names, versions) exactly as written.
- Write the title in the same language as the message.
- If the message is only a greeting or small talk, the title is \"Greeting\".

Examples:
debug 500 errors in production -> Production 500 errors
how do I add a dark mode toggle to my react app -> Dark mode toggle
refactor the auth middleware to use async/await -> Auth middleware async refactor
hi -> Greeting
why is `cargo test` failing with E0432 in src/lib.rs -> Cargo test E0432 error
write a python script that renames my photos by date -> Photo rename script
Kannst du mir helfen, die Datenbankmigration zu schreiben? -> Datenbankmigration schreiben";

/// The message the title is written from: the opening request, plus the latest one for long conversations.
pub fn request_text(first: &str, latest: Option<&str>) -> String {
    let cut = |s: &str| -> String {
        let s = s.trim();
        if s.chars().count() > 1500 { format!("{}...", s.chars().take(1500).collect::<String>()) } else { s.to_string() }
    };
    match latest.filter(|l| l.trim() != first.trim()) {
        Some(l) => format!("Title the conversation that began with the first message, taking the latest message into account.\n\n<first message>\n{}\n</first message>\n\n<latest message>\n{}\n</latest message>", cut(first), cut(l)),
        None => format!("Title the conversation that begins with the message below. Output only the title.\n\n<message>\n{}\n</message>", cut(first)),
    }
}

/// Make a model reply safe to store as a title; `None` if nothing usable is left.
pub fn clean(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut t = line.trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '*' | '#' | '\u{201c}' | '\u{201d}')).trim();
    if t.len() >= 6 && t[..6].eq_ignore_ascii_case("title:") {
        t = t[6..].trim().trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '*' | '\u{201c}' | '\u{201d}')).trim();
    }
    let t = t.trim_end_matches('.').trim();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() {
        return None;
    }
    Some(if t.chars().count() > 80 { t.chars().take(80).collect::<String>().trim_end().to_string() } else { t })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleaning_model_replies() {
        for (raw, want) in [
            ("Production 500 errors", Some("Production 500 errors")),
            ("\"Dark mode toggle\"", Some("Dark mode toggle")),
            ("Title: Auth middleware refactor.", Some("Auth middleware refactor")),
            ("title: \"Cargo test E0432 error\"", Some("Cargo test E0432 error")),
            ("**Photo rename script**\n\nThis title names the task.", Some("Photo rename script")),
            ("\n\n  `Greeting`  ", Some("Greeting")),
            ("# Heading style", Some("Heading style")),
            ("Too   many    spaces", Some("Too many spaces")),
            ("\u{201c}Smart quotes\u{201d}", Some("Smart quotes")),
            ("", None),
            ("  \n ", None),
            ("\"\"", None),
            ("Title:", None),
        ] {
            assert_eq!(clean(raw).as_deref(), want, "{raw:?}");
        }
        let long = "word ".repeat(40);
        assert!(clean(&long).unwrap().chars().count() <= 80);
    }

    #[test]
    fn request_text_variants() {
        let one = request_text("fix the build", None);
        assert!(one.contains("<message>\nfix the build\n</message>") && !one.contains("latest"));
        assert!(!request_text("same", Some("same")).contains("latest"), "a repeated message is not added twice");
        let two = request_text("fix the build", Some("now add tests"));
        assert!(two.contains("<first message>") && two.contains("now add tests"));
        assert!(request_text(&"x".repeat(5000), None).len() < 1700, "very long messages are cut");
    }
}
