//! Markdown files with a small YAML-ish frontmatter (skills, custom commands, agents).
//!
//! Deliberately lenient (docs/08: "if YAML fails, retry treating unquoted values containing `:` as block scalars"):
//! only flat `key: value` pairs are understood, a value is everything after the first colon, so descriptions
//! like `Use when: the user asks` just work. Quoted values are unquoted; `>` and `|` introduce an indented block
//! (folded with spaces or kept as lines); other indented lines continue the previous value.
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Doc {
    pub meta: BTreeMap<String, String>,
    pub body: String,
}

impl Doc {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.meta.get(key).map(String::as_str).filter(|v| !v.is_empty())
    }
    pub fn flag(&self, key: &str) -> bool {
        matches!(self.get(key).map(str::to_lowercase).as_deref(), Some("true" | "yes" | "on" | "1"))
    }
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    for q in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            let inner = &v[1..v.len() - 1];
            return if q == '"' { inner.replace("\\\"", "\"").replace("\\n", "\n") } else { inner.replace("''", "'") };
        }
    }
    v.to_string()
}

pub fn parse(text: &str) -> Doc {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let norm = text.replace("\r\n", "\n");
    let Some(rest) = norm.strip_prefix("---\n") else { return Doc { meta: BTreeMap::new(), body: norm } };
    // The header ends at the first line that is exactly `---`.
    let (header, body) = match rest.split_once("\n---\n").or_else(|| rest.strip_suffix("\n---").map(|h| (h, ""))) {
        Some((h, b)) => (h, b),
        None if rest.starts_with("---\n") => ("", &rest[4..]),
        None => return Doc { meta: BTreeMap::new(), body: norm },
    };
    let mut meta: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<(String, Option<char>)> = None; // (key, block style)
    for line in header.lines() {
        let indented = line.starts_with([' ', '\t']);
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if indented {
            if let Some((key, style)) = &current {
                let v = meta.entry(key.clone()).or_default();
                if !v.is_empty() {
                    v.push(if *style == Some('|') { '\n' } else { ' ' });
                }
                v.push_str(line.trim());
            }
            continue;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        let k = k.trim();
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            continue;
        }
        let v = v.trim();
        let block = match v {
            ">" | ">-" | ">+" => Some('>'),
            "|" | "|-" | "|+" => Some('|'),
            _ => None,
        };
        meta.insert(k.to_string(), if block.is_some() { String::new() } else { unquote(v) });
        current = Some((k.to_string(), block));
    }
    Doc { meta, body: body.trim_start_matches('\n').to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_frontmatter_and_body() {
        let d = parse("---\nname: pdf-tools\ndescription: Work with PDFs\nsubagent: true\n---\n# Title\n\nbody text\n");
        assert_eq!((d.get("name"), d.get("description"), d.flag("subagent"), d.flag("missing")), (Some("pdf-tools"), Some("Work with PDFs"), true, false));
        assert_eq!(d.body, "# Title\n\nbody text\n");
    }

    #[test]
    fn colons_quotes_and_blocks_are_tolerated() {
        let d = parse(
            "---\ndescription: Use when: the user asks for X, Y: or Z\nquoted: \"say \\\"hi\\\": now\"\nsingle: 'it''s fine'\nfolded: >\n  first line\n  second line\nliteral: |\n  keep\n  lines\nwrapped: starts here\n  and continues\n# comment\nnot a pair\nbad key!: ignored\n---\nbody",
        );
        assert_eq!(d.get("description"), Some("Use when: the user asks for X, Y: or Z"));
        assert_eq!(d.get("quoted"), Some("say \"hi\": now"));
        assert_eq!(d.get("single"), Some("it's fine"));
        assert_eq!(d.get("folded"), Some("first line second line"));
        assert_eq!(d.get("literal"), Some("keep\nlines"));
        assert_eq!(d.get("wrapped"), Some("starts here and continues"));
        assert_eq!(d.meta.len(), 6);
        assert_eq!(d.body, "body");
    }

    #[test]
    fn no_frontmatter_or_unterminated_means_all_body() {
        assert_eq!(parse("just text\n"), Doc { meta: Default::default(), body: "just text\n".into() });
        let d = parse("---\nname: x\nno closing fence");
        assert!(d.meta.is_empty() && d.body.contains("no closing fence"));
        assert_eq!(parse("---\n---\nbody").body, "body");
        assert_eq!(parse("\u{feff}---\r\nname: bom\r\n---\r\nhi").get("name"), Some("bom"), "BOM and CRLF");
        assert_eq!(parse("---\nname: only\n---").body, "");
    }
}
