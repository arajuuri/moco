//! A small Markdown renderer for assistant text: headings, lists, quotes, fenced code, tables, inline
//! code/bold/italic/links. Line breaks in the source are kept (chat replies rely on them). No dependencies;
//! unknown or half-written syntax (the reply is still streaming) falls back to plain text.
use super::app::Theme;
use ratatui::prelude::*;
use unicode_width::UnicodeWidthChar;

type Cell = (char, Style);

fn clean(text: &str) -> String {
    text.replace('\t', "    ").chars().filter(|c| !c.is_control() || *c == '\n').collect()
}

fn width_of(c: char) -> usize {
    c.width().unwrap_or(0)
}

fn cells_width(cells: &[Cell]) -> usize {
    cells.iter().map(|(c, _)| width_of(*c)).sum()
}

/// Word wrap of styled characters.
fn wrap_cells(cells: &[Cell], width: usize) -> Vec<Vec<Cell>> {
    let width = width.max(1);
    let mut out: Vec<Vec<Cell>> = vec![];
    let mut line: Vec<Cell> = vec![];
    let (mut w, mut last_space) = (0, None::<usize>);
    for &(c, st) in cells {
        let cw = width_of(c);
        if w + cw > width {
            if c == ' ' {
                out.push(std::mem::take(&mut line));
                w = 0;
                last_space = None;
                continue;
            }
            match last_space {
                Some(i) if i > 0 => {
                    let rest = line.split_off(i + 1);
                    line.pop(); // the space
                    out.push(std::mem::take(&mut line));
                    line = rest;
                    w = cells_width(&line);
                }
                _ => {
                    out.push(std::mem::take(&mut line));
                    w = 0;
                }
            }
            last_space = None;
        }
        if c == ' ' {
            last_space = Some(line.len());
        }
        line.push((c, st));
        w += cw;
    }
    out.push(line);
    out
}

fn to_line(prefix: &[Cell], cells: &[Cell]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![];
    let mut buf = String::new();
    let mut cur: Option<Style> = None;
    for &(c, st) in prefix.iter().chain(cells.iter()) {
        if cur != Some(st) {
            if let Some(s) = cur {
                spans.push(Span::styled(std::mem::take(&mut buf), s));
            }
            cur = Some(st);
        }
        buf.push(c);
    }
    if let Some(s) = cur {
        spans.push(Span::styled(buf, s));
    }
    Line::from(spans)
}

/// Wrap `cells` to `width`, with `first` before the first line and `rest` before the others (both the same width).
fn emit(out: &mut Vec<Line<'static>>, first: &[Cell], rest: &[Cell], cells: &[Cell], width: usize) {
    let avail = width.saturating_sub(cells_width(first)).max(1);
    for (n, l) in wrap_cells(cells, avail).iter().enumerate() {
        out.push(to_line(if n == 0 { first } else { rest }, l));
    }
}

fn plain(s: &str, st: Style) -> Vec<Cell> {
    s.chars().map(|c| (c, st)).collect()
}

/// Inline spans: `code`, **bold**, *italic*, [text](url).
fn inline(s: &str, base: Style, t: &Theme) -> Vec<Cell> {
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<Cell> = vec![];
    let mut i = 0;
    let find = |from: usize, pat: &[char]| -> Option<usize> { (from..chars.len().saturating_sub(pat.len() - 1)).find(|&j| chars[j..j + pat.len()] == *pat) };
    while i < chars.len() {
        let c = chars[i];
        let prev_word = i > 0 && chars[i - 1].is_alphanumeric();
        if c == '`' {
            let ticks = chars[i..].iter().take_while(|&&x| x == '`').count();
            let pat = vec!['`'; ticks];
            if let Some(end) = find(i + ticks, &pat) {
                out.extend(chars[i + ticks..end].iter().map(|&x| (x, t.code)));
                i = end + ticks;
                continue;
            }
        } else if (c == '*' || c == '_') && chars.get(i + 1) == Some(&c) && !chars.get(i + 2).is_none_or(|x| x.is_whitespace()) && !(c == '_' && prev_word) {
            if let Some(end) = find(i + 2, &[c, c]).filter(|&e| e > i + 2) {
                let inner: String = chars[i + 2..end].iter().collect();
                out.extend(inline(&inner, base.add_modifier(Modifier::BOLD), t));
                i = end + 2;
                continue;
            }
        } else if (c == '*' || c == '_') && !chars.get(i + 1).is_none_or(|x| x.is_whitespace() || *x == c) && !(c == '_' && prev_word) && !(c == '*' && chars.get(i + 1) == Some(&' ')) {
            if let Some(end) = find(i + 1, &[c]).filter(|&e| e > i + 1 && !chars[e - 1].is_whitespace() && !(c == '_' && chars.get(e + 1).is_some_and(|x| x.is_alphanumeric()))) {
                let inner: String = chars[i + 1..end].iter().collect();
                out.extend(inline(&inner, base.add_modifier(Modifier::ITALIC), t));
                i = end + 1;
                continue;
            }
        } else if c == '[' {
            if let Some(close) = find(i + 1, &[']']).filter(|&e| chars.get(e + 1) == Some(&'(')) {
                if let Some(end) = find(close + 2, &[')']) {
                    let label: String = chars[i + 1..close].iter().collect();
                    let url: String = chars[close + 2..end].iter().collect();
                    out.extend(inline(&label, t.link, t));
                    if !url.is_empty() && url != label {
                        out.extend(plain(&format!(" ({url})"), t.dim));
                    }
                    i = end + 1;
                    continue;
                }
            }
        }
        out.push((c, base));
        i += 1;
    }
    out
}

fn list_marker(line: &str) -> Option<(usize, String, &str)> {
    let indent = line.len() - line.trim_start().len();
    let rest = &line[indent..];
    for m in ["- ", "* ", "+ "] {
        if let Some(r) = rest.strip_prefix(m) {
            return Some((indent, "• ".to_string(), r));
        }
    }
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if (1..=9).contains(&digits) {
        if let Some(r) = rest[digits..].strip_prefix(". ").or_else(|| rest[digits..].strip_prefix(") ")) {
            return Some((indent, format!("{}. ", &rest[..digits]), r));
        }
    }
    None
}

fn split_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

fn is_separator(line: &str) -> bool {
    let cells = split_row(line);
    !cells.is_empty() && cells.iter().all(|c| !c.is_empty() && c.chars().all(|x| matches!(x, '-' | ':' | ' ')) && c.contains('-'))
}

fn table(rows: &[String], width: usize, t: &Theme, out: &mut Vec<Line<'static>>) {
    let header = split_row(&rows[0]);
    let body: Vec<Vec<String>> = rows[2..].iter().map(|r| split_row(r)).collect();
    let n = header.len();
    let styled = |s: &str, st: Style| inline(s, st, t);
    let mut cols: Vec<usize> = (0..n).map(|c| cells_width(&styled(&header[c], t.text))).collect();
    for r in &body {
        for (c, w) in cols.iter_mut().enumerate() {
            *w = (*w).max(r.get(c).map_or(0, |x| cells_width(&styled(x, t.text))));
        }
    }
    let total: usize = cols.iter().sum::<usize>() + 3 * n + 1;
    if total > width {
        // Too wide to align: one line per row, cells separated by " | ".
        for (k, r) in std::iter::once(&header).chain(body.iter()).enumerate() {
            let mut cells = vec![];
            for (j, c) in r.iter().enumerate() {
                if j > 0 {
                    cells.extend(plain(" | ", t.dim));
                }
                cells.extend(styled(c, if k == 0 { t.text.add_modifier(Modifier::BOLD) } else { t.text }));
            }
            emit(out, &[], &[], &cells, width);
        }
        return;
    }
    let row_line = |r: &[String], st: Style| {
        let mut cells = plain("|", t.dim);
        for (c, w) in cols.iter().enumerate() {
            let content = styled(r.get(c).map_or("", |x| x.as_str()), st);
            let pad = w - cells_width(&content);
            cells.extend(plain(" ", t.text));
            cells.extend(content);
            cells.extend(plain(&" ".repeat(pad + 1), t.text));
            cells.extend(plain("|", t.dim));
        }
        to_line(&[], &cells)
    };
    out.push(row_line(&header, t.text.add_modifier(Modifier::BOLD)));
    let rule: String = std::iter::once("|".to_string()).chain(cols.iter().map(|w| format!("{}|", "-".repeat(w + 2)))).collect();
    out.push(Line::styled(rule, t.dim));
    for r in &body {
        out.push(row_line(r, t.text));
    }
}

pub fn render(text: &str, width: usize, t: &Theme) -> Vec<Line<'static>> {
    let text = clean(text);
    let lines: Vec<&str> = text.split('\n').collect();
    let width = width.max(8);
    let mut out: Vec<Line<'static>> = vec![];
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        // fenced code
        if let Some(fence) = ["```", "~~~"].iter().find(|f| trimmed.starts_with(**f)) {
            let lang = trimmed.trim_start_matches(fence.chars().next().unwrap()).trim();
            if !lang.is_empty() {
                out.push(Line::styled(format!("  {lang}"), t.dim));
            }
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with(fence) {
                // Code is never reflowed: long lines are cut into pieces.
                let cells = plain(lines[i], t.code);
                let prefix = plain("  ", t.code);
                let mut rest: &[Cell] = &cells;
                loop {
                    let mut w = 0;
                    let take = rest.iter().take_while(|(c, _)| { w += width_of(*c); w <= width - 2 }).count().max(1.min(rest.len()));
                    out.push(to_line(&prefix, &rest[..take]));
                    rest = &rest[take..];
                    if rest.is_empty() {
                        break;
                    }
                }
                i += 1;
            }
            i += 1; // the closing fence (or the end of a half-streamed block)
            continue;
        }
        // table: a header row, a separator row, then rows
        if line.contains('|') && i + 1 < lines.len() && is_separator(lines[i + 1]) && split_row(line).len() == split_row(lines[i + 1]).len() {
            let mut j = i + 2;
            while j < lines.len() && lines[j].contains('|') && !lines[j].trim().is_empty() {
                j += 1;
            }
            let rows: Vec<String> = lines[i..j].iter().map(|s| s.to_string()).collect();
            table(&rows, width, t, &mut out);
            i = j;
            continue;
        }
        if trimmed.is_empty() {
            out.push(Line::default());
        } else if let Some(h) = trimmed.strip_prefix('#').filter(|_| trimmed.starts_with('#')) {
            let level = 1 + h.chars().take_while(|&c| c == '#').count();
            let body = h.trim_start_matches('#');
            if level <= 6 && body.starts_with(' ') {
                let st = if level <= 2 { t.heading.add_modifier(Modifier::BOLD).add_modifier(Modifier::UNDERLINED) } else { t.heading.add_modifier(Modifier::BOLD) };
                emit(&mut out, &[], &[], &inline(body.trim(), st, t), width);
            } else {
                emit(&mut out, &[], &[], &inline(line, t.text, t), width);
            }
        } else if trimmed.len() >= 3 && trimmed.chars().all(|c| c == '-' || c == '*' || c == '_') && trimmed.chars().all(|c| c == trimmed.chars().next().unwrap()) {
            out.push(Line::styled("-".repeat(width.min(40)), t.dim));
        } else if let Some(q) = trimmed.strip_prefix('>') {
            let bar = plain("| ", t.quote);
            emit(&mut out, &bar, &bar, &inline(q.trim_start(), t.quote, t), width);
        } else if let Some((indent, marker, rest)) = list_marker(line) {
            let pad = " ".repeat(indent.min(width / 2));
            let first = plain(&format!("{pad}{marker}"), t.text);
            let cont = plain(&" ".repeat(pad.len() + marker.chars().count()), t.text);
            emit(&mut out, &first, &cont, &inline(rest, t.text, t), width);
        } else {
            let lead = line.len() - trimmed.len();
            let pad = plain(&" ".repeat(lead.min(width / 2)), t.text);
            emit(&mut out, &pad, &pad, &inline(trimmed, t.text, t), width);
        }
        i += 1;
    }
    while out.last().is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty())) {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::default()
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    fn styled<'a>(lines: &'a [Line<'static>], word: &str) -> Option<&'a Span<'static>> {
        lines.iter().flat_map(|l| l.spans.iter()).find(|s| s.content.contains(word))
    }

    #[test]
    fn headings_lists_quotes_and_rules() {
        let t = theme();
        let l = render("# Title\n\n- one\n- two with a long line that has to wrap around\n  - nested\n1. first\n2) second\n> quoted\n---\nplain", 24, &t);
        assert_eq!(
            text(&l),
            vec!["Title", "", "• one", "• two with a long line", "  that has to wrap", "  around", "  • nested", "1. first", "2. second", "| quoted", "------------------------", "plain"]
        );
        assert!(styled(&l, "Title").unwrap().style.add_modifier.contains(Modifier::BOLD));
        // "#hashtag" and "####### seven" are not headings.
        let l = render("#hashtag\n####### seven", 40, &t);
        assert_eq!(text(&l), vec!["#hashtag", "####### seven"]);
    }

    #[test]
    fn inline_styles_and_links() {
        let t = theme();
        let l = render("use `cargo test` and **bold _inner_** or *it* plus [docs](https://x.y/z) and snake_case_name, 5 * 6 = 30", 100, &t);
        assert_eq!(text(&l), vec!["use cargo test and bold inner or it plus docs (https://x.y/z) and snake_case_name, 5 * 6 = 30"]);
        assert_eq!(styled(&l, "cargo test").unwrap().style, t.code);
        let bold = styled(&l, "bold ").unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        assert!(styled(&l, "inner").unwrap().style.add_modifier.contains(Modifier::ITALIC | Modifier::BOLD));
        assert!(styled(&l, "it").is_some());
        assert_eq!(styled(&l, "docs").unwrap().style, t.link);
        // Unclosed markers stay literal (the reply may still be streaming).
        assert_eq!(text(&render("half **bold and `code", 80, &t)), vec!["half **bold and `code"]);
    }

    #[test]
    fn fenced_code_is_not_reflowed_and_survives_an_unclosed_fence() {
        let t = theme();
        let l = render("text\n```rust\nlet a = 1;\nlet very_long_identifier_name = compute(1, 2, 3);\n```\nafter", 20, &t);
        let s = text(&l);
        assert_eq!(s[..3], ["text", "  rust", "  let a = 1;"]);
        assert_eq!(s.last().unwrap(), "after");
        assert!(s.iter().all(|x| x.chars().count() <= 20), "{s:?}");
        assert!(s.iter().any(|x| x == "  let very_long_iden"), "{s:?}");
        assert_eq!(styled(&l, "let a").unwrap().style, t.code);
        // A fence that is still being streamed shows what has arrived.
        assert_eq!(text(&render("```\nfn main() {", 40, &t)), vec!["  fn main() {"]);
    }

    #[test]
    fn tables_align_or_fall_back_when_too_wide() {
        let t = theme();
        let src = "| Name | Qty |\n|---|---:|\n| apple | 3 |\n| kiwi | 12 |";
        assert_eq!(text(&render(src, 40, &t)), vec!["| Name  | Qty |", "|-------|-----|", "| apple | 3   |", "| kiwi  | 12  |"]);
        assert_eq!(text(&render(src, 12, &t)), vec!["Name | Qty", "apple | 3", "kiwi | 12"]);
        // A lone pipe line is just text.
        assert_eq!(text(&render("a | b", 40, &t)), vec!["a | b"]);
    }

    #[test]
    fn wide_characters_wrap_by_display_width_and_controls_are_dropped() {
        let t = theme();
        assert_eq!(text(&render("日本語のテキスト", 8, &t)), vec!["日本語の", "テキスト"]);
        assert_eq!(text(&render("a\x1b[31mred\x07\tb", 40, &t)), vec!["a[31mred    b"]);
        assert!(render("", 10, &t).is_empty());
        assert_eq!(text(&render("para one\n\npara two\n\n\n", 40, &t)), vec!["para one", "", "para two"]);
    }
}
