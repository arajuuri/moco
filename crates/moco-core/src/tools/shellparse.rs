//! v1 shell command scanning (docs/05-permissions.md "Simplification for v1"): quote-aware split on
//! `;`, `&&`, `||`, `|`, `&`, newline. Command substitution (`$(...)`, backticks, `<(...)`) and
//! unterminated quotes are not parsed: they mark the result `opaque`, which forces an ask.
//!
//! Heredocs (`<<WORD`, `<<'WORD'`, `<<"WORD"`, `<<-WORD`) are input to the command that owns them, not commands: the
//! body is consumed up to the delimiter line and appended to that command's text, so `osascript *` covers
//! `osascript <<'EOF' ...`. Anything where this parser and the shell could disagree about where the body ends is
//! opaque instead of guessed.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Normalized command text (raw words, single-spaced, leading `VAR=x` assignments removed).
    pub text: String,
    /// Unquoted words, same filtering.
    pub words: Vec<String>,
    /// Directory targets of cd/pushd/popd/chdir.
    pub dirs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parsed {
    pub commands: Vec<Command>,
    pub opaque: bool,
}

/// Number of leading words that identify a command for "always allow" patterns.
/// Partial table: extend from upstream `shell/parse.ts` `ARITY` when that source is at hand.
const ARITY: &[(&str, usize)] = &[
    ("git", 2), ("docker", 2), ("kubectl", 2), ("npm", 2), ("npm run", 3), ("pnpm", 2), ("pnpm run", 3),
    ("yarn", 2), ("bun", 2), ("bun run", 3), ("cargo", 2), ("cargo add", 3), ("go", 2), ("brew", 2),
    ("pip", 2), ("pip3", 2), ("systemctl", 2), ("aws", 3), ("az", 3),
];

pub fn arity(words: &[String]) -> usize {
    ARITY
        .iter()
        .filter(|(k, _)| {
            let kw: Vec<&str> = k.split(' ').collect();
            words.len() >= kw.len() && kw.iter().zip(words).all(|(a, b)| a == b)
        })
        .map(|(k, n)| (k.split(' ').count(), *n))
        .max()
        .map(|(_, n)| n)
        .unwrap_or(1)
}

/// `git push *`-style pattern persisted by an "always" reply.
pub fn save_pattern(words: &[String]) -> String {
    let n = arity(words).min(words.len());
    format!("{} *", words[..n].join(" "))
}

fn is_assignment(w: &str) -> bool {
    match w.split_once('=') {
        Some((k, _)) => !k.is_empty() && k.chars().enumerate().all(|(i, c)| c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())),
        None => false,
    }
}

/// Variables that change which executable, library, startup file or helper program runs.
fn is_risky_assignment(w: &str) -> bool {
    let k = w.split_once('=').map_or(w, |(k, _)| k);
    k.starts_with("LD_") || k.starts_with("DYLD_") || k.starts_with("BASH_FUNC_") || k.starts_with("GIT_") || k.starts_with("NPM_CONFIG_")
        || matches!(
            k,
            "PATH" | "BASH_ENV" | "ENV" | "IFS" | "SHELLOPTS" | "BASHOPTS" | "PS4" | "PROMPT_COMMAND" | "CDPATH" | "GLOBIGNORE"
                | "PYTHONPATH" | "PYTHONSTARTUP" | "PYTHONHOME" | "NODE_OPTIONS" | "NODE_PATH" | "RUBYOPT" | "RUBYLIB"
                | "PERL5OPT" | "PERL5LIB" | "RUSTC" | "RUSTC_WRAPPER" | "RUSTFLAGS" | "PAGER" | "MANPAGER" | "EDITOR"
                | "VISUAL" | "LESSOPEN" | "SSH_ASKPASS" | "HOME" | "SHELL"
        )
}

fn has_output_redirection(words: &[(String, String)]) -> bool {
    for (i, (raw, _)) in words.iter().enumerate() {
        let cs: Vec<char> = raw.chars().collect();
        let (mut quote, mut escaped) = (None, false);
        for (j, c) in cs.iter().enumerate() {
            if escaped {
                escaped = false;
                continue;
            }
            if *c == '\\' && quote != Some('\'') {
                escaped = true;
                continue;
            }
            match quote {
                Some(q) if *c == q => quote = None,
                Some(_) => {}
                None if *c == '\'' || *c == '"' => quote = Some(*c),
                None if *c == '>' => {
                    let mut end = j + 1;
                    while cs.get(end) == Some(&'>') {
                        end += 1;
                    }
                    if cs.get(end) == Some(&'|') {
                        end += 1;
                    }
                    if cs.get(end) == Some(&'&') && cs.get(end + 1).is_some_and(char::is_ascii_digit) {
                        continue;
                    }
                    let suffix: String = cs[end..].iter().collect();
                    let target = if suffix.is_empty() {
                        words.get(i + 1).map(|(_, v)| v.as_str()).unwrap_or("")
                    } else {
                        suffix.trim_matches(['\'', '"'])
                    };
                    if target != "/dev/null" {
                        return true;
                    }
                }
                None => {}
            }
        }
    }
    false
}

fn is_in_place_writer(words: &[String]) -> bool {
    let Some(program) = words.first().and_then(|w| std::path::Path::new(w).file_name()).and_then(|s| s.to_str()) else { return false };
    if program != "sed" && program != "perl" {
        return false;
    }
    words.iter().skip(1).any(|w| {
        w == "--in-place" || w == "-i" || w == "-pi" || w == "-ip" || w.starts_with("-i") || w.starts_with("-pi") || w.starts_with("-ip")
    })
}

fn is_python(program: &str) -> bool {
    let name = std::path::Path::new(program).file_name().and_then(|s| s.to_str()).unwrap_or(program);
    name == "python" || name.strip_prefix("python").is_some_and(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

fn python_opens_for_write(source: &str) -> bool {
    let cs: Vec<char> = source.chars().collect();
    let mut i = 0;
    while i + 4 <= cs.len() {
        if cs[i..i + 4] != ['o', 'p', 'e', 'n'] || (i > 0 && (cs[i - 1].is_ascii_alphanumeric() || cs[i - 1] == '_')) {
            i += 1;
            continue;
        }
        let mut start = i + 4;
        while cs.get(start).is_some_and(|c| c.is_whitespace()) {
            start += 1;
        }
        if cs.get(start) != Some(&'(') {
            i += 4;
            continue;
        }
        let (mut depth, mut quote, mut escaped, mut commas) = (1usize, None, false, 0usize);
        let mut arg = start + 1;
        let mut mode = String::new();
        for j in start + 1..cs.len() {
            let c = cs[j];
            if escaped {
                escaped = false;
                continue;
            }
            if c == '\\' && quote.is_some() {
                escaped = true;
                continue;
            }
            if let Some(q) = quote {
                if c == q {
                    quote = None;
                }
                continue;
            }
            if c == '\'' || c == '"' {
                quote = Some(c);
            } else if matches!(c, '(' | '[' | '{') {
                depth += 1;
            } else if matches!(c, ')' | ']' | '}') {
                depth -= 1;
                if depth == 0 {
                    if commas == 1 {
                        mode.extend(&cs[arg..j]);
                    }
                    break;
                }
            } else if c == ',' && depth == 1 {
                if commas == 1 {
                    mode.extend(&cs[arg..j]);
                    break;
                }
                commas += 1;
                arg = j + 1;
            }
        }
        let mode = mode.trim();
        let mode = mode.split_once('=').filter(|(key, _)| key.trim() == "mode").map_or(mode, |(_, value)| value.trim());
        let mode = if mode.len() >= 2
            && matches!(mode.chars().next(), Some('\'' | '"'))
            && mode.chars().next() == mode.chars().last()
        {
            &mode[1..mode.len() - 1]
        } else {
            mode
        };
        if matches!(mode.chars().next(), Some('w' | 'a' | 'x')) || mode.contains('+') {
            return true;
        }
        i = start + 1;
    }
    false
}

fn python_writes(command: &Command) -> bool {
    if !command.words.first().is_some_and(|w| is_python(w)) {
        return false;
    }
    let mut source = command.words.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
    if let Some((_, body)) = command.text.split_once('\n') {
        source.push('\n');
        source.push_str(body);
    }
    python_opens_for_write(&source)
}

/// A `<<` seen on the current line whose body starts after the line's newline.
struct Heredoc {
    delimiter: String,
    /// `<<-`: leading tabs are stripped from body lines and the delimiter line.
    strip_tabs: bool,
    /// Any quoting in the delimiter makes the body literal; otherwise the shell expands `$(...)` and backticks in it.
    quoted: bool,
    /// Index in `Parsed::commands` of the command the body belongs to.
    owner: usize,
}

/// Reads a heredoc delimiter word starting at `cs[j]`: `(end, unquoted delimiter, quoted)`, or `None` when it is
/// missing or not something to guess about (unterminated quotes, `$'...'`, expansions).
fn heredoc_delimiter(cs: &[char], mut j: usize) -> Option<(usize, String, bool)> {
    let (mut word, mut quoted) = (String::new(), false);
    while j < cs.len() {
        match cs[j] {
            ' ' | '\t' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')' => break,
            '\'' => {
                quoted = true;
                let close = (j + 1..cs.len()).find(|&k| cs[k] == '\'')?;
                word.extend(&cs[j + 1..close]);
                j = close;
            }
            '"' => {
                quoted = true;
                j += 1;
                loop {
                    match cs.get(j)? {
                        '"' => break,
                        '\\' if matches!(cs.get(j + 1), Some('"' | '\\' | '$' | '`')) => {
                            word.push(cs[j + 1]);
                            j += 1;
                        }
                        '$' | '`' => return None,
                        c => word.push(*c),
                    }
                    j += 1;
                }
            }
            '\\' => {
                quoted = true;
                word.push(*cs.get(j + 1)?);
                j += 1;
            }
            '$' | '`' => return None,
            c => word.push(c),
        }
        j += 1;
    }
    (!word.is_empty()).then_some((j, word, quoted))
}

pub fn parse(input: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let mut heredocs: Vec<Heredoc> = vec![];
    let cs: Vec<char> = input.chars().collect();
    let mut raw_words: Vec<(String, String)> = vec![]; // (raw, unquoted)
    let (mut raw, mut val) = (String::new(), String::new());
    let mut in_word = false;
    let (mut sq, mut dq) = (false, false);
    let mut i = 0;

    let flush_word = |raw: &mut String, val: &mut String, in_word: &mut bool, words: &mut Vec<(String, String)>| {
        if *in_word {
            words.push((std::mem::take(raw), std::mem::take(val)));
            *in_word = false;
        }
    };
    let finish = |words: &mut Vec<(String, String)>, parsed: &mut Parsed| {
        let skip = words.iter().take_while(|(_, v)| is_assignment(v)).count();
        let risky = words.iter().take(skip).any(|(_, v)| is_risky_assignment(v));
        if risky {
            parsed.opaque = true;
        }
        // Risky assignments stay in the text; harmless ones are dropped as before.
        let kept = if risky { 0 } else { skip };
        let has_redirect = has_output_redirection(words);
        let all: Vec<(String, String)> = std::mem::take(words);
        let text = all.iter().skip(kept).map(|(r, _)| r.as_str()).collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            return;
        }
        let mut w: Vec<String> = all.into_iter().skip(skip).map(|(_, v)| v).collect();
        if w.is_empty() {
            // Assignment-only command (`PATH=./x`): it still changes later commands.
            w = text.split(' ').map(String::from).collect();
        }
        let dirs = if matches!(w[0].as_str(), "cd" | "pushd" | "popd" | "chdir") {
            w.get(1).filter(|a| !a.starts_with('-')).cloned().into_iter().collect()
        } else {
            vec![]
        };
        if has_redirect || is_in_place_writer(&w) {
            parsed.opaque = true;
        }
        parsed.commands.push(Command { text, words: w, dirs });
    };

    while i < cs.len() {
        let c = cs[i];
        if sq {
            raw.push(c);
            if c == '\'' { sq = false } else { val.push(c) }
        } else if dq {
            match c {
                '"' => { dq = false; raw.push(c); }
                '\\' if i + 1 < cs.len() => { raw.push(c); raw.push(cs[i + 1]); val.push(cs[i + 1]); i += 1; }
                '`' => { parsed.opaque = true; raw.push(c); val.push(c); }
                '$' if cs.get(i + 1) == Some(&'(') => { parsed.opaque = true; raw.push(c); val.push(c); }
                _ => { raw.push(c); val.push(c); }
            }
        } else {
            match c {
                '\'' => { sq = true; in_word = true; raw.push(c); }
                '"' => { dq = true; in_word = true; raw.push(c); }
                '\\' if i + 1 < cs.len() => { in_word = true; raw.push(c); raw.push(cs[i + 1]); val.push(cs[i + 1]); i += 1; }
                '`' => { parsed.opaque = true; in_word = true; raw.push(c); val.push(c); }
                '$' if cs.get(i + 1) == Some(&'(') => { parsed.opaque = true; in_word = true; raw.push(c); val.push(c); }
                '<' | '>' if cs.get(i + 1) == Some(&'(') => { parsed.opaque = true; in_word = true; raw.push(c); val.push(c); }
                '<' if cs.get(i + 1) == Some(&'<') && cs.get(i + 2) != Some(&'<') => {
                    // A heredoc operator is its own word, except for a file descriptor in front of it (`0<<EOF`).
                    if !raw.chars().all(|c| c.is_ascii_digit()) {
                        flush_word(&mut raw, &mut val, &mut in_word, &mut raw_words);
                    }
                    let strip_tabs = cs.get(i + 2) == Some(&'-');
                    let mut j = i + 2 + usize::from(strip_tabs);
                    while matches!(cs.get(j), Some(' ' | '\t')) {
                        j += 1;
                    }
                    let op = if strip_tabs { "<<-" } else { "<<" };
                    in_word = true;
                    raw.push_str(op);
                    val.push_str(op);
                    match heredoc_delimiter(&cs, j) {
                        Some((end, delimiter, quoted)) => {
                            raw.extend(&cs[j..end]);
                            val.push_str(&delimiter);
                            flush_word(&mut raw, &mut val, &mut in_word, &mut raw_words);
                            heredocs.push(Heredoc { delimiter, strip_tabs, quoted, owner: parsed.commands.len() });
                            i = end;
                            continue;
                        }
                        // No delimiter we can be sure of: leave the rest to the normal scan, and ask.
                        None => {
                            parsed.opaque = true;
                            i += 2 + usize::from(strip_tabs);
                            continue;
                        }
                    }
                }
                '<' if cs.get(i + 1) == Some(&'<') => {
                    // `<<<` here-string: an ordinary word follows.
                    in_word = true;
                    raw.push_str("<<<");
                    val.push_str("<<<");
                    i += 3;
                    continue;
                }
                ' ' | '\t' | '\r' => flush_word(&mut raw, &mut val, &mut in_word, &mut raw_words),
                '\n' | ';' => {
                    flush_word(&mut raw, &mut val, &mut in_word, &mut raw_words);
                    finish(&mut raw_words, &mut parsed);
                    if c == '\n' && !heredocs.is_empty() {
                        i = read_heredoc_bodies(&cs, i + 1, std::mem::take(&mut heredocs), &mut parsed);
                        continue;
                    }
                }
                '&' | '|' => {
                    // `2>&1` and `>&2` are redirections, not separators.
                    let redirect = c == '&' && (raw.ends_with('>') || raw.ends_with('<'));
                    if redirect {
                        in_word = true;
                        raw.push(c);
                        val.push(c);
                    } else {
                        flush_word(&mut raw, &mut val, &mut in_word, &mut raw_words);
                        finish(&mut raw_words, &mut parsed);
                        if cs.get(i + 1) == Some(&c) {
                            i += 1; // && or ||
                        }
                    }
                }
                _ => { in_word = true; raw.push(c); val.push(c); }
            }
        }
        i += 1;
    }
    if sq || dq {
        // Unterminated quote: do not guess. One resource, forced ask.
        return Parsed { commands: vec![Command { text: input.trim().to_string(), words: input.split_whitespace().map(String::from).collect(), dirs: vec![] }], opaque: true };
    }
    flush_word(&mut raw, &mut val, &mut in_word, &mut raw_words);
    finish(&mut raw_words, &mut parsed);
    if !heredocs.is_empty() {
        // `cat <<EOF` with no body at all: the shell reads to end of input. Nothing to guess from, so ask.
        parsed.opaque = true;
    }
    if parsed.commands.iter().any(python_writes) {
        parsed.opaque = true;
    }
    parsed
}

/// Consumes the bodies of `heredocs`, in order, from the line starting at `cs[start]`, appends each body and its
/// delimiter line to the owning command's text, and returns where scanning resumes.
fn read_heredoc_bodies(cs: &[char], start: usize, heredocs: Vec<Heredoc>, parsed: &mut Parsed) -> usize {
    let mut pos = start;
    for h in heredocs {
        let body_start = pos;
        let mut terminated = false;
        while pos < cs.len() {
            let end = (pos..cs.len()).find(|&k| cs[k] == '\n').unwrap_or(cs.len());
            let line: String = cs[pos..end].iter().collect();
            let next = (end + 1).min(cs.len());
            let compared = if h.strip_tabs { line.trim_start_matches('\t') } else { line.as_str() };
            if compared == h.delimiter {
                terminated = true;
                append_body(parsed, h.owner, &cs[body_start..end]);
                pos = next;
                break;
            }
            if !h.quoted {
                // The shell expands substitutions in an unquoted body. It also joins a line ending in a backslash
                // with the next one before comparing it with the delimiter, so `EO\` + `F` would end the body
                // there; rather than replicate that, ask.
                if line.contains("$(") || line.contains('`') || line.ends_with('\\') {
                    parsed.opaque = true;
                }
            }
            pos = next;
        }
        if !terminated {
            // Runs to the end of the input: the shell accepts that with a warning; we ask instead.
            parsed.opaque = true;
            append_body(parsed, h.owner, &cs[body_start..cs.len()]);
            return cs.len();
        }
    }
    pos
}

fn append_body(parsed: &mut Parsed, owner: usize, body: &[char]) {
    match parsed.commands.get_mut(owner) {
        Some(c) => {
            c.text.push('\n');
            c.text.extend(body);
        }
        None => parsed.opaque = true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(cmd: &str) -> Vec<String> {
        parse(cmd).commands.into_iter().map(|c| c.text).collect()
    }

    #[test]
    fn splits_on_operators() {
        assert_eq!(texts("ls -la && git status; echo hi | wc -l || true\nmake"), ["ls -la", "git status", "echo hi", "wc -l", "true", "make"]);
    }

    #[test]
    fn quotes_protect_operators() {
        assert_eq!(texts(r#"echo "a && b" 'c ; d' e\;f"#), [r#"echo "a && b" 'c ; d' e\;f"#]);
        assert!(!parse(r#"echo "a && b""#).opaque);
    }

    #[test]
    fn redirections_are_not_separators() {
        assert_eq!(texts("make 2>&1 | tee out.log"), ["make 2>&1", "tee out.log"]);
        assert_eq!(texts("cmd >&2"), ["cmd >&2"]);
    }

    #[test]
    fn env_assignments() {
        assert!(parse("FOO=bar").commands.is_empty());
        assert_eq!(texts("FOO=bar BAZ=1 cargo test"), ["cargo test"]);
    }

    #[test]
    fn risky_env_assignments_are_kept_and_force_ask() {
        for c in ["PATH=./attacker-bin ls", "LD_PRELOAD=/tmp/x.so ls", "DYLD_INSERT_LIBRARIES=/tmp/x.dylib ls", "FOO=1 PATH=. ls", "BASH_ENV=/tmp/x ls", "PATH='./a' ls", "GIT_SSH_COMMAND=x ls"] {
            let p = parse(c);
            assert!(p.opaque, "{c}");
            assert_eq!(p.commands.len(), 1, "{c}");
            assert!(p.commands[0].text.contains('='), "{c}: {}", p.commands[0].text);
            assert_eq!(p.commands[0].words, ["ls"], "{c}");
        }
        assert_eq!(texts("PATH=./attacker-bin ls"), ["PATH=./attacker-bin ls"]);
        // Assignment-only commands persist in the shell, so they are not dropped.
        assert_eq!(texts("PATH=./x; ls"), ["PATH=./x", "ls"]);
        assert!(parse("PATH=./x; ls").opaque);
        assert!(!parse("FOO=bar cargo test").opaque);
    }

    #[test]
    fn substitutions_and_bad_quotes_are_opaque() {
        for c in ["echo $(rm -rf x)", "echo `id`", "diff <(a) <(b)", "echo \"$(id)\"", "echo 'unterminated", "echo \"unterminated"] {
            assert!(parse(c).opaque, "{c}");
        }
        assert!(!parse("echo '$(not substitution)'").opaque, "single quotes are literal");
        assert_eq!(texts("echo 'unterminated"), ["echo 'unterminated"], "whole command is one resource");
    }

    #[test]
    fn cd_targets() {
        let p = parse("cd /tmp/x && ls; pushd ../y; cd -");
        let dirs: Vec<_> = p.commands.iter().flat_map(|c| c.dirs.clone()).collect();
        assert_eq!(dirs, ["/tmp/x", "../y"]);
    }

    #[test]
    fn a_heredoc_body_belongs_to_its_command() {
        // Issue #84: every body line used to be a command of its own.
        let cmd = "osascript <<'EOF' 2>&1\ntell application \"System Events\" to tell process \"Chess\"\n  set g to group 1 of window 1\n  click button \"white knight, b1\" of g\nend tell\nEOF";
        let p = parse(cmd);
        assert!(!p.opaque);
        assert_eq!(p.commands.len(), 1, "{:?}", p.commands);
        assert_eq!(p.commands[0].text, cmd, "the body is part of the resource, so rules and the prompt see it");
        assert_eq!(p.commands[0].words, ["osascript", "<<EOF", "2>&1"]);
        assert_eq!(save_pattern(&p.commands[0].words), "osascript *");
        assert!(crate::wildcard::matches(&p.commands[0].text, "osascript *"), "a saved `osascript *` covers it");

        // An apostrophe in the body is data, not an unterminated quote.
        let p = parse("python3 <<'PY'\nprint(\"don't\")\nPY");
        assert!(!p.opaque);
        assert_eq!(p.commands[0].words, ["python3", "<<PY"]);

        // Commands around it are still found: before on the same line, after the delimiter line, and in a pipe.
        assert_eq!(
            texts("cd x && cat <<EOF > out.txt\nhello\nEOF\nls; git commit -F - <<\"MSG\"\nfix: y\nMSG\necho done"),
            ["cd x", "cat <<EOF > out.txt\nhello\nEOF", "ls", "git commit -F - <<\"MSG\"\nfix: y\nMSG", "echo done"]
        );
        assert_eq!(texts("cat <<EOF | grep a\na\nb\nEOF"), ["cat <<EOF\na\nb\nEOF", "grep a"]);
        assert_eq!(texts("cat << \\EOF\nrm -rf /\nEOF"), ["cat <<\\EOF\nrm -rf /\nEOF"], "space after << and a backslash-quoted word");
    }

    #[test]
    fn heredoc_variants() {
        // <<- strips leading tabs from body and delimiter lines.
        assert_eq!(texts("cat <<-EOF\n\tbody\n\tEOF\nls"), ["cat <<-EOF\n\tbody\n\tEOF", "ls"]);
        // Two heredocs on one line take their bodies in order, even for different commands.
        assert_eq!(texts("cat <<A; cat <<B\none\nA\ntwo\nB\nls"), ["cat <<A\none\nA", "cat <<B\ntwo\nB", "ls"]);
        assert_eq!(texts("diff /dev/fd/3 - <<A 3<<B\n1\nA\n2\nB"), ["diff /dev/fd/3 - <<A 3<<B\n1\nA\n2\nB"]);
        // `<<<` is a here-string: no body follows.
        assert_eq!(texts("grep x <<< \"$v\"\nls"), ["grep x <<< \"$v\"", "ls"]);
        // A delimiter line with anything else on it does not end the body (the shell agrees).
        let p = parse("cat <<EOF\nEOF \nrm -rf x\nEOF\nls");
        assert_eq!(p.commands.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(), ["cat <<EOF\nEOF \nrm -rf x\nEOF", "ls"]);
        // A quoted body is literal; an unquoted one expands substitutions.
        assert!(!parse("cat <<'EOF'\n$(id) `id`\nEOF").opaque);
        assert!(!parse("cat <<\"EOF\"\n$(id)\nEOF").opaque);
        assert!(!parse("cat <<EOF\n$HOME ${USER}\nEOF").opaque, "plain variables are fine");
        assert!(parse("cat <<EOF\n$(id)\nEOF").opaque);
        assert!(parse("cat <<EOF\n`id`\nEOF").opaque);
    }

    #[test]
    fn heredocs_the_shell_could_read_differently_are_opaque() {
        for c in [
            // No delimiter line: the body runs to the end of input.
            "cat <<EOF\nrm -rf x",
            "cat <<EOF",
            // In an unquoted body the shell joins `EO\` and `F` into the delimiter, ending the body early; a parser that
            // did not would take the following lines as data and hide them.
            "cat <<EOF\nEO\\\nF\nrm -rf x\nEOF",
            // No usable delimiter.
            "cat <<",
            "cat << ;ls",
            "cat <<'EOF\nx\nEOF",
            "cat <<$X\nx\n$X",
            "cat <<$'E'\nx\nE",
        ] {
            assert!(parse(c).opaque, "{c:?}");
        }
    }

    #[test]
    fn save_patterns_use_arity() {
        let pat = |c: &str| save_pattern(&parse(c).commands[0].words);
        assert_eq!(pat("git push origin main"), "git push *");
        assert_eq!(pat("npm run dev -- --port 1"), "npm run dev *");
        assert_eq!(pat("cargo build --release"), "cargo build *");
        assert_eq!(pat("ls -la /tmp"), "ls *");
        assert_eq!(pat("git"), "git *");
        assert_eq!(pat("aws s3 ls"), "aws s3 ls *");
    }

    #[test]
    fn obvious_file_writes_force_a_prompt_but_program_execution_does_not() {
        for c in [
            "sed -i 's/old/new/' file.txt",
            "perl -pi -e 's/old/new/' file.txt",
            "python3 -c \"open('file.txt', 'w').write('x')\"",
            "python3 <<'PY'\nopen('file.txt', 'w').write('x')\nPY",
            "printf x > file.txt",
            "printf x >>file.txt",
            "printf x 2>file.txt",
        ] {
            assert!(parse(c).opaque, "{c}");
        }
        for c in [
            "python3 script.py",
            "cargo test",
            "printf x > /dev/null",
            "printf '>'",
            "printf x 2>&1",
        ] {
            assert!(!parse(c).opaque, "{c}");
        }
    }
}
