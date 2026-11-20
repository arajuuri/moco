//! Wildcard matching for permission patterns.
//! `*` matches anything including `/`, `?` matches one char, whole-string match, `\` normalized to `/`.
//! A pattern ending in ` *` also matches the bare command (`ls *` matches `ls`).

fn norm(s: &str) -> Vec<char> {
    s.chars().map(|c| if c == '\\' { '/' } else { c }).collect()
}

fn glob(text: &[char], pat: &[char]) -> bool {
    let (mut t, mut p) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while t < text.len() {
        if p < pat.len() && (pat[p] == '?' || (pat[p] != '*' && pat[p] == text[t])) {
            t += 1;
            p += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    pat[p..].iter().all(|&c| c == '*')
}

pub fn matches(text: &str, pattern: &str) -> bool {
    let (t, p) = (norm(text), norm(pattern));
    if glob(&t, &p) {
        return true;
    }
    match pattern.strip_suffix(" *") {
        Some(bare) => glob(&t, &norm(bare)),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn table() {
        let cases = [
            ("anything", "*", true),
            ("src/a/b.rs", "src/*", true), // * crosses /
            ("src/a/b.rs", "*.rs", true),
            ("src/a/b.rs", "*.ts", false),
            ("a.env", "*.env", true),
            ("a.env.example", "*.env.*", true),
            ("a.env.example", "*.env.example", true),
            ("abc", "a?c", true),
            ("ac", "a?c", false),
            ("ls", "ls *", true), // bare command
            ("ls -la", "ls *", true),
            ("lsof", "ls *", false),
            ("git push origin main", "git push *", true),
            ("git pull", "git push *", false),
            ("a.b", "a.b", true),
            ("axb", "a.b", false), // metachar is literal
            ("src\\a.rs", "src/*", true), // backslash normalized
            ("", "*", true),
            ("", "?", false),
            ("abcabc", "*abc", true),
            ("abcabd", "*abc", false),
        ];
        for (text, pat, want) in cases {
            assert_eq!(matches(text, pat), want, "matches({text:?}, {pat:?})");
        }
    }
}
