//! v1 shell command scanning (docs/05-permissions.md "Simplification for v1"): quote-aware split on
//! `;`, `&&`, `||`, `|`, `&`, newline. Command substitution (`$(...)`, backticks, `<(...)`) and
//! unterminated quotes are not parsed: they mark the result `opaque`, which forces an ask.

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

pub fn parse(input: &str) -> Parsed {
    let mut parsed = Parsed::default();
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
        let all: Vec<(String, String)> = words.drain(..).collect();
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
                ' ' | '\t' | '\r' => flush_word(&mut raw, &mut val, &mut in_word, &mut raw_words),
                '\n' | ';' => {
                    flush_word(&mut raw, &mut val, &mut in_word, &mut raw_words);
                    finish(&mut raw_words, &mut parsed);
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
    parsed
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
    fn save_patterns_use_arity() {
        let pat = |c: &str| save_pattern(&parse(c).commands[0].words);
        assert_eq!(pat("git push origin main"), "git push *");
        assert_eq!(pat("npm run dev -- --port 1"), "npm run dev *");
        assert_eq!(pat("cargo build --release"), "cargo build *");
        assert_eq!(pat("ls -la /tmp"), "ls *");
        assert_eq!(pat("git"), "git *");
        assert_eq!(pat("aws s3 ls"), "aws s3 ls *");
    }
}
