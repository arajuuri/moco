//! Small HTML to Markdown / plain-text converter for `webfetch`. Not a full HTML parser: it tokenizes tags,
//! skips non-content elements and maps the common structure (headings, paragraphs, lists, links, code, tables).

#[derive(Debug)]
enum Tok {
    Text(String),
    Open { name: String, attrs: Vec<(String, String)>, self_close: bool },
    Close(String),
}

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find(';').filter(|e| *e <= 10);
        let decoded = end.and_then(|e| {
            let name = &tail[1..e];
            let ch = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some(' '),
                "mdash" => Some('\u{2014}'),
                "ndash" => Some('\u{2013}'),
                "hellip" => Some('\u{2026}'),
                "copy" => Some('\u{a9}'),
                n if n.starts_with("#x") || n.starts_with("#X") => u32::from_str_radix(&n[2..], 16).ok().and_then(char::from_u32),
                n if n.starts_with('#') => n[1..].parse::<u32>().ok().and_then(char::from_u32),
                _ => None,
            };
            ch.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &tail[e + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn tokenize(html: &str) -> Vec<Tok> {
    let b = html.as_bytes();
    let (mut i, mut toks, mut text_start) = (0usize, vec![], 0usize);
    let flush = |toks: &mut Vec<Tok>, from: usize, to: usize| {
        if to > from {
            toks.push(Tok::Text(decode_entities(&html[from..to])));
        }
    };
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &html[i..];
        if rest.starts_with("<!--") {
            flush(&mut toks, text_start, i);
            i += rest.find("-->").map(|e| e + 3).unwrap_or(rest.len());
            text_start = i;
        } else if rest.starts_with("<!") || rest.starts_with("<?") {
            flush(&mut toks, text_start, i);
            i += rest.find('>').map(|e| e + 1).unwrap_or(rest.len());
            text_start = i;
        } else if rest.starts_with("</") && rest.as_bytes().get(2).is_some_and(|c| c.is_ascii_alphabetic()) {
            flush(&mut toks, text_start, i);
            let end = rest.find('>').unwrap_or(rest.len() - 1);
            toks.push(Tok::Close(rest[2..end].trim().to_lowercase()));
            i += end + 1;
            text_start = i;
        } else if rest.as_bytes().get(1).is_some_and(|c| c.is_ascii_alphabetic()) {
            flush(&mut toks, text_start, i);
            // Find the closing '>' outside quotes.
            let (mut j, mut quote) = (1usize, 0u8);
            let rb = rest.as_bytes();
            while j < rb.len() {
                match (quote, rb[j]) {
                    (0, b'"') | (0, b'\'') => quote = rb[j],
                    (q, c) if q != 0 && c == q => quote = 0,
                    (0, b'>') => break,
                    _ => {}
                }
                j += 1;
            }
            let inner = &rest[1..j.min(rest.len())];
            let self_close = inner.ends_with('/');
            let inner = inner.trim_end_matches('/');
            let name_end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
            let name = inner[..name_end].to_lowercase();
            let attrs = parse_attrs(&inner[name_end..]);
            let raw_text = matches!(name.as_str(), "script" | "style");
            i += j + 1;
            toks.push(Tok::Open { name: name.clone(), attrs, self_close });
            if raw_text && !self_close {
                // Everything up to the matching close tag is code, not content.
                let lower = html[i..].to_lowercase();
                let close = lower.find(&format!("</{name}")).unwrap_or(lower.len());
                i += close;
            }
            text_start = i;
        } else {
            i += 1;
        }
    }
    flush(&mut toks, text_start, b.len());
    toks
}

fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    let cs: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        while i < cs.len() && (cs[i].is_whitespace() || cs[i] == '/') {
            i += 1;
        }
        let start = i;
        while i < cs.len() && !cs[i].is_whitespace() && cs[i] != '=' && cs[i] != '/' {
            i += 1;
        }
        if start == i {
            break;
        }
        let name: String = cs[start..i].iter().collect::<String>().to_lowercase();
        while i < cs.len() && cs[i].is_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < cs.len() && cs[i] == '=' {
            i += 1;
            while i < cs.len() && cs[i].is_whitespace() {
                i += 1;
            }
            if i < cs.len() && (cs[i] == '"' || cs[i] == '\'') {
                let q = cs[i];
                i += 1;
                while i < cs.len() && cs[i] != q {
                    value.push(cs[i]);
                    i += 1;
                }
                i += 1;
            } else {
                while i < cs.len() && !cs[i].is_whitespace() {
                    value.push(cs[i]);
                    i += 1;
                }
            }
        }
        out.push((name, decode_entities(&value)));
    }
    out
}

const SKIP: &[&str] = &["head", "script", "style", "noscript", "svg", "template", "title", "iframe", "object"];
const VOID: &[&str] = &["br", "hr", "img", "input", "meta", "link", "area", "base", "col", "embed", "source", "track", "wbr"];
const BLOCK: &[&str] = &["div", "section", "article", "main", "header", "footer", "nav", "aside", "form", "figure", "figcaption", "details", "summary", "dl", "dt", "dd", "address"];

struct Conv {
    out: String,
    md: bool,
    lists: Vec<(bool, usize)>,
    links: Vec<(String, usize)>,
    pre: usize,
    quote: usize,
    skip: Option<(String, usize)>,
}

impl Conv {
    fn newline(&mut self) {
        self.out.push('\n');
        if self.md && self.quote > 0 {
            self.out.push_str(&"> ".repeat(self.quote));
        }
    }
    fn line_break(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.newline();
        }
    }
    fn block_break(&mut self) {
        if self.out.is_empty() {
            return;
        }
        self.line_break();
        let blank = self.out.ends_with("\n\n") || (self.quote > 0 && self.out.trim_end_matches(['>', ' ']).ends_with("\n\n"));
        if !blank && !self.out.trim_end().is_empty() {
            self.newline();
        }
    }
    fn text(&mut self, t: &str) {
        if self.pre > 0 {
            self.out.push_str(&t.replace('\n', if self.md && self.quote > 0 { "\n> " } else { "\n" }));
            return;
        }
        let mut s = String::new();
        let mut prev_space = false;
        for c in t.chars() {
            if c.is_whitespace() {
                if !prev_space {
                    s.push(' ');
                }
                prev_space = true;
            } else {
                s.push(c);
                prev_space = false;
            }
        }
        if self.out.is_empty() || self.out.ends_with('\n') || self.out.ends_with(' ') || self.out.ends_with("> ") {
            s = s.trim_start().to_string();
        }
        self.out.push_str(&s);
    }
    fn mark(&mut self, m: &str) {
        if self.md {
            self.out.push_str(m);
        }
    }
}

fn convert(html: &str, md: bool) -> String {
    let mut c = Conv { out: String::new(), md, lists: vec![], links: vec![], pre: 0, quote: 0, skip: None };
    for tok in tokenize(html) {
        if let Some((name, depth)) = &mut c.skip {
            match &tok {
                Tok::Open { name: n, self_close: false, .. } if n == name => *depth += 1,
                Tok::Close(n) if n == name => {
                    *depth -= 1;
                    if *depth == 0 {
                        c.skip = None;
                    }
                }
                _ => {}
            }
            continue;
        }
        match tok {
            Tok::Text(t) => c.text(&t),
            Tok::Open { name, attrs, self_close } => {
                let attr = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
                if SKIP.contains(&name.as_str()) {
                    if !self_close && !VOID.contains(&name.as_str()) {
                        c.skip = Some((name, 1));
                    }
                    continue;
                }
                match name.as_str() {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        c.block_break();
                        let n = (name.as_bytes()[1] - b'0') as usize;
                        c.mark(&format!("{} ", "#".repeat(n)));
                    }
                    "p" | "pre" | "ul" | "ol" | "table" => {
                        if (name == "ul" || name == "ol") && !c.lists.is_empty() {
                            c.line_break();
                        } else {
                            c.block_break();
                        }
                        if name == "pre" {
                            c.mark("```\n");
                            c.pre += 1;
                        }
                        if name == "ul" || name == "ol" {
                            c.lists.push((name == "ol", 0));
                        }
                    }
                    "blockquote" => {
                        c.block_break();
                        c.quote += 1;
                        c.mark("> ");
                    }
                    "li" => {
                        c.line_break();
                        let depth = c.lists.len().saturating_sub(1);
                        let marker = match c.lists.last_mut() {
                            Some((true, n)) => {
                                *n += 1;
                                format!("{}. ", n)
                            }
                            _ => "- ".to_string(),
                        };
                        c.out.push_str(&"  ".repeat(depth));
                        c.out.push_str(&marker);
                    }
                    "br" => c.newline(),
                    "hr" => {
                        c.block_break();
                        c.mark("---");
                        c.block_break();
                    }
                    "tr" => {
                        c.line_break();
                        c.mark("| ");
                    }
                    "a" => {
                        let href = attr("href").filter(|h| !h.is_empty() && !h.starts_with("javascript:") && !h.starts_with('#'));
                        c.links.push((href.unwrap_or_default(), c.out.len()));
                    }
                    "img" => {
                        let alt = attr("alt").unwrap_or_default();
                        if md {
                            if let Some(src) = attr("src") {
                                c.out.push_str(&format!("![{alt}]({src})"));
                            }
                        } else {
                            c.text(&alt);
                        }
                    }
                    "strong" | "b" => c.mark("**"),
                    "em" | "i" => c.mark("*"),
                    "code" if c.pre == 0 => c.mark("`"),
                    n if BLOCK.contains(&n) => c.line_break(),
                    _ => {}
                }
            }
            Tok::Close(name) => match name.as_str() {
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "p" | "table" => c.block_break(),
                "pre" => {
                    c.pre = c.pre.saturating_sub(1);
                    c.line_break();
                    c.mark("```");
                    c.block_break();
                }
                "ul" | "ol" => {
                    c.lists.pop();
                    if c.lists.is_empty() {
                        c.block_break();
                    }
                }
                "blockquote" => {
                    c.quote = c.quote.saturating_sub(1);
                    c.block_break();
                }
                "li" => c.line_break(),
                "tr" => {
                    c.out = c.out.trim_end().to_string();
                    c.line_break();
                }
                "td" | "th" => {
                    c.out = c.out.trim_end().to_string();
                    c.mark(" | ");
                }
                "a" => {
                    if let Some((href, start)) = c.links.pop() {
                        if md && !href.is_empty() {
                            let label = c.out[start..].trim().to_string();
                            c.out.truncate(start);
                            if !label.is_empty() {
                                c.out.push_str(&format!("[{label}]({href})"));
                            }
                        }
                    }
                }
                "strong" | "b" => c.mark("**"),
                "em" | "i" => c.mark("*"),
                "code" if c.pre == 0 => c.mark("`"),
                n if BLOCK.contains(&n) => c.line_break(),
                _ => {}
            },
        }
    }
    // Tidy: trailing spaces off, at most one blank line in a row.
    let mut lines: Vec<&str> = vec![];
    let mut blank = 0;
    let mut in_fence = false;
    for l in c.out.split('\n') {
        let l = if in_fence { l } else { l.trim_end() };
        if l.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if l.trim().is_empty() && !in_fence {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        lines.push(l);
    }
    lines.join("\n").trim().to_string()
}

pub fn to_markdown(html: &str) -> String {
    convert(html, true)
}

pub fn to_text(html: &str) -> String {
    convert(html, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!DOCTYPE html><html><head><title>Ignore me</title><style>p{color:red}</style><script>var x = "<p>nope</p>";</script></head>
<body><!-- a comment --><h1>Main &amp; Title</h1>
<p>Some <strong>bold</strong> and <em>italic</em> text with a <a href="https://example.com/a?b=1&amp;c=2">link</a>
spanning   lines, plus <code>inline()</code>.</p>
<ul><li>one</li><li>two<ul><li>nested</li></ul></li></ul>
<ol><li>first</li><li>second</li></ol>
<pre><code>fn main() {
    println!("hi");
}</code></pre>
<blockquote>quoted text</blockquote>
<img src="/img.png" alt="a pic"><hr>
<table><tr><th>Name</th><th>Qty</th></tr><tr><td>apple</td><td>3</td></tr></table>
<div>tail &#169; &lt;done&gt;</div></body></html>"#;

    #[test]
    fn markdown_conversion() {
        let md = to_markdown(PAGE);
        let expect = "# Main & Title

Some **bold** and *italic* text with a [link](https://example.com/a?b=1&c=2) spanning lines, plus `inline()`.

- one
- two
  - nested

1. first
2. second

```
fn main() {
    println!(\"hi\");
}
```

> quoted text

![a pic](/img.png)

---

| Name | Qty |
| apple | 3 |

tail \u{a9} <done>";
        assert_eq!(md, expect);
        assert!(!md.contains("Ignore me") && !md.contains("nope") && !md.contains("color:red"));
    }

    #[test]
    fn plain_text_conversion_drops_markup() {
        let t = to_text("<h2>Head</h2><p>See <a href=\"/x\">the docs</a> for <b>more</b>.</p><ul><li>a</li><li>b</li></ul><img alt=\"logo\" src=\"l.png\">");
        assert_eq!(t, "Head\n\nSee the docs for more.\n\n- a\n- b\n\nlogo");
    }

    #[test]
    fn robust_to_junk() {
        assert_eq!(to_markdown("plain text only"), "plain text only");
        assert_eq!(to_markdown("a < b and c > d"), "a < b and c > d");
        assert_eq!(to_markdown("<p>unclosed <b>bold"), "unclosed **bold");
        assert_eq!(to_markdown("<a href=\"javascript:void(0)\">click</a> <a href=\"#top\">top</a>"), "click top");
        assert_eq!(to_markdown("<p>&unknown; &#x41; &#66;</p>"), "&unknown; A B");
        assert_eq!(to_markdown(""), "");
        assert!(to_markdown("<script>never closed <p>x").is_empty());
    }
}
