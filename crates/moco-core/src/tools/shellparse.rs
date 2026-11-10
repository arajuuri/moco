//! v1 shell command scanning (docs/05-permissions.md "Simplification for v1"): quote-aware split on
//! `;`, `&&`, `||`, `|`, `&`, newline. Command substitution (`$(...)`, backticks, `<(...)`) and
//! unterminated quotes are not parsed: they mark the result `opaque`, which forces an ask.
//!
//! Heredocs (`<<WORD`, `<<'WORD'`, `<<"WORD"`, `<<-WORD`) are input to the command that owns them, not commands: the
//! body is consumed up to the delimiter line and appended to that command's text, so `osascript *` covers
//! `osascript <<'EOF' ...`. Anything where this parser and the shell could disagree about where the body ends is
//! opaque instead of guessed.
//!
//! Rules match command text, so a command whose text hides what runs (`'git' push`, `/bin/rm`, `env rm`, `then rm`,
//! `git -C . push`, `sudo rm`, `bash -c 'rm ...'`, `find -exec rm`) is followed by the command that really runs, as an
//! extra entry with unquoted words and the program name normalized (issue #118). Each entry is a resource, and a
//! request is allowed only when every resource is, so the extra entries can only add asks and denies.
//!
//! Constructs where the shell (bash, sh or zsh, whichever `$SHELL` is) reads more into the text than this parser does
//! are opaque: arithmetic (`((...))`, `$((...))`, `$[...]`, `let`, `[[ x -eq y ]]`, `declare -i`), a `(` anywhere but
//! at the start of a command (zsh glob qualifiers `*(e:...:)`, zsh `=(...)`, extglob, arrays, function definitions),
//! zsh `${(e)...}` and bash `${x@P}`, and a program name built from `$`, globs or braces.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Normalized command text (raw words, single-spaced, leading `VAR=x` assignments removed). For a command that
    /// another one runs (see the module docs), the unquoted words.
    pub text: String,
    /// Unquoted words, same filtering.
    pub words: Vec<String>,
    /// Places that get the external-directory check: directory targets of cd/pushd/popd/chdir and `git
    /// -C`/`--git-dir`/`--work-tree`, input redirection targets, and the files and directories that read commands
    /// (`cat`, `head`, `grep`, `rg`...) read (issue #117).
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

/// `git push *`-style pattern persisted by an "always" reply. A pattern never covers more code than the user saw:
/// - Ordinary commands: the arity prefix plus ` *` (`ls *`, `git push *`, `npm run dev *`). If a word inside the prefix
///   is an option (`npm -g install`, `bun -e ...`), the prefix does not name a subcommand, and the command is saved exactly.
/// - git: global options are kept as typed and the subcommand follows (`git -C sub status *`); bare `git` is exact.
/// - Wrappers (`env`, `xargs`, `nohup`, `timeout 5`, `time`...) and keywords (`then`, `!`...): their own words exactly,
///   then the pattern of the command they run (`timeout 5 cargo test *`), never `env *` or `xargs *`.
/// - Interpreters and shells: `python3 x.py *`, `python3 -m pytest *`, `bash run.sh *`, so only that script's
///   arguments vary. With options or code on the command line or on stdin (`-c`, `-e`, `<<EOF`) the command is
///   saved exactly.
/// - Programs whose arguments are code or commands (`find`, `awk`, `eval`, `source`, `watch`) and `sudo`/`doas`: exact.
///
/// "Exactly" means the unquoted words joined by spaces, with `*` and `?` turned into `?` so they cannot widen it.
/// Such a pattern matches the command again only when it is typed without quotes or a heredoc body; otherwise the
/// "always" answer does not stick, which is the safe way to be wrong.
pub fn save_pattern(words: &[String]) -> String {
    pattern(words).unwrap_or_else(|| exact(words))
}

fn exact(words: &[String]) -> String {
    words.iter().map(|w| w.replace(['*', '?'], "?")).collect::<Vec<_>>().join(" ")
}

/// `None` when the command can only be saved exactly.
fn pattern(words: &[String]) -> Option<String> {
    let first = words.first()?;
    let prefix = if RESERVED.contains(&first.as_str()) { Some(1) } else { redirection_words(first) };
    if let Some(k) = prefix {
        let k = k.min(words.len());
        return Some(format!("{} {}", exact(&words[..k]), pattern(words.get(k..).filter(|r| !r.is_empty())?)?));
    }
    let name = program_name(first);
    if matches!(name, "sudo" | "doas" | "su") {
        return None;
    }
    if is_wrapper(name) {
        let k = 1 + wrapper_end(name, &words[1..])?.0;
        let inner = words.get(k..).filter(|r| !r.is_empty())?;
        return Some(format!("{} {}", exact(&words[..k]), pattern(inner)?));
    }
    if name == "git" {
        let g = git_globals(&words[1..])?.0;
        words.get(1 + g)?;
        return Some(format!("{} *", exact(&words[..2 + g])));
    }
    if CODE_ARGS.contains(&name) {
        return None;
    }
    let plain = |w: &String| !w.is_empty() && !w.starts_with(['-', '+', '<', '>']);
    if is_interpreter(name) || is_shell(name) || matches!(name, "npx" | "pnpx" | "bunx" | "uvx") {
        let n = if is_python(name) && words.get(1).is_some_and(|w| w == "-m") { 3 } else { 2 };
        return words.get(n - 1).filter(|script| plain(script)).map(|_| format!("{} *", exact(&words[..n])));
    }
    let n = arity(words).min(words.len());
    if words[1..n].iter().any(|w| w.starts_with('-')) {
        return None;
    }
    Some(format!("{} *", exact(&words[..n])))
}

/// Keywords that can stand before a command without changing what it runs.
const RESERVED: &[&str] = &["!", "{", "then", "do", "else", "elif", "if", "while", "until"];

/// Programs whose arguments are themselves code or commands.
const CODE_ARGS: &[&str] = &["find", "awk", "gawk", "nawk", "mawk", "eval", "source", ".", "watch", "parallel", "expect"];

fn is_shell(name: &str) -> bool {
    matches!(name, "sh" | "bash" | "zsh" | "dash" | "ksh" | "mksh" | "fish" | "csh" | "tcsh")
}

fn is_interpreter(name: &str) -> bool {
    is_python(name) || matches!(name, "node" | "nodejs" | "ruby" | "perl" | "php" | "lua" | "luajit" | "osascript" | "Rscript" | "tclsh" | "wish" | "pwsh" | "deno")
}

/// The program a command word names: the basename of a path, and zsh's `=rm` (the path of `rm`) as `rm`.
fn program_name(word: &str) -> &str {
    let name = word.rsplit('/').next().unwrap_or(word);
    match name.strip_prefix('=') {
        Some(rest) if !rest.is_empty() => rest,
        _ => name,
    }
}

/// Options of a wrapper: single-letter flags, letters that take an argument (attached or as the next word), letters
/// whose argument can only be attached, and the long forms.
struct Opts {
    flags: &'static str,
    args: &'static str,
    attached: &'static str,
    long_flags: &'static [&'static str],
    long_args: &'static [&'static str],
}

fn wrapper_opts(name: &str) -> Option<Opts> {
    let o = |flags, args, long_flags, long_args| Opts { flags, args, attached: "", long_flags, long_args };
    Some(match name {
        "env" => o("i0v", "uC", &["ignore-environment", "null"], &["unset", "chdir"]),
        "command" => o("pvV", "", &[], &[]),
        "exec" => o("cl", "a", &[], &[]),
        "time" => o("plv", "of", &["portability", "verbose", "append"], &["output", "format"]),
        "nice" => o("", "n", &[], &["adjustment"]),
        "timeout" | "gtimeout" => o("v", "sk", &["foreground", "preserve-status", "verbose"], &["signal", "kill-after"]),
        "stdbuf" | "gstdbuf" => o("", "ioe", &[], &["input", "output", "error"]),
        "caffeinate" => o("dimsu", "tw", &[], &[]),
        "xargs" | "gxargs" => Opts {
            flags: "0tprxo",
            args: "ILnPsEdaJR",
            attached: "ile",
            long_flags: &["null", "verbose", "no-run-if-empty", "exit", "interactive", "open-tty"],
            long_args: &["max-args", "max-procs", "delimiter", "arg-file", "replace", "max-lines", "max-chars", "eof"],
        },
        "sudo" => o("EHnkSbPAB", "ugpCDhrtTU", &["preserve-env", "non-interactive", "set-home", "background", "stdin"], &["user", "group", "prompt", "chdir", "host", "role", "type", "close-from", "other-user"]),
        "doas" => o("n", "u", &[], &[]),
        "builtin" | "nohup" | "noglob" | "nocorrect" => o("", "", &[], &[]),
        _ => return None,
    })
}

/// Programs that run the command given in their arguments.
fn is_wrapper(name: &str) -> bool {
    wrapper_opts(name).is_some()
}

/// How many of `rest` (the words after the wrapper) belong to the wrapper, and whether one of them is a risky
/// `env` assignment. `None` for an option this parser does not know (it may take an argument, or be `env -S`, which
/// splits a string into a new command line), so nothing after it can be trusted to be the command.
fn wrapper_end(name: &str, rest: &[String]) -> Option<(usize, bool)> {
    let o = wrapper_opts(name)?;
    let mut j = 0;
    while let Some(w) = rest.get(j) {
        if w == "--" {
            j += 1;
            break;
        }
        if name == "env" && w == "-" {
            j += 1;
            continue;
        }
        if let Some(long) = w.strip_prefix("--") {
            let (key, inline) = long.split_once('=').map_or((long, false), |(k, _)| (k, true));
            if o.long_flags.contains(&key) {
                j += 1;
            } else if o.long_args.contains(&key) {
                j += if inline { 1 } else { 2 };
            } else {
                return None;
            }
            continue;
        }
        let Some(letters) = w.strip_prefix('-').filter(|l| !l.is_empty()) else { break };
        if name == "nice" && letters.chars().all(|c| c.is_ascii_digit()) {
            j += 1;
            continue;
        }
        let mut step = 1;
        for (k, c) in letters.char_indices() {
            if o.flags.contains(c) {
                continue;
            }
            if o.args.contains(c) {
                if k + c.len_utf8() == letters.len() {
                    step = 2;
                }
                break;
            }
            if o.attached.contains(c) {
                break;
            }
            return None;
        }
        j += step;
    }
    let mut j = j.min(rest.len());
    let mut risky = false;
    if name == "env" {
        while rest.get(j).is_some_and(|w| is_assignment(w)) {
            risky |= is_risky_assignment(&rest[j]);
            j += 1;
        }
    }
    if matches!(name, "timeout" | "gtimeout") && j < rest.len() {
        j += 1; // the duration
    }
    Some((j, risky))
}

/// git's global options before the subcommand: how many words they take, and the directories they point at. `None`
/// for options that run other programs (`-c alias.x='!cmd'`, `-c core.pager=...`, `--config-env`, `--exec-path=`)
/// and for options this parser does not know.
fn git_globals(rest: &[String]) -> Option<(usize, Vec<String>)> {
    const FLAGS: &[&str] = &[
        "-p", "-P", "--paginate", "--no-pager", "--bare", "--no-replace-objects", "--literal-pathspecs", "--glob-pathspecs",
        "--noglob-pathspecs", "--icase-pathspecs", "--no-optional-locks", "--no-advice", "--html-path", "--man-path",
        "--info-path", "--version", "--help", "-v", "-h",
    ];
    const ARGS: &[&str] = &["-C", "--git-dir", "--work-tree", "--namespace", "--super-prefix", "--list-cmds", "--attr-source"];
    const DIRS: &[&str] = &["-C", "--git-dir", "--work-tree"];
    let (mut j, mut dirs) = (0, vec![]);
    while let Some(w) = rest.get(j) {
        if !w.starts_with('-') {
            break;
        }
        if FLAGS.contains(&w.as_str()) {
            j += 1;
        } else if ARGS.contains(&w.as_str()) {
            let arg = rest.get(j + 1)?;
            if DIRS.contains(&w.as_str()) {
                dirs.push(arg.clone());
            }
            j += 2;
        } else if let Some((key, value)) = w.split_once('=').filter(|(k, _)| ARGS.contains(k)) {
            if DIRS.contains(&key) {
                dirs.push(value.to_string());
            }
            j += 1;
        } else {
            return None;
        }
    }
    Some((j, dirs))
}

/// The script of `sh -c SCRIPT` (also `bash -lc`, `zsh -o x -c`): `Ok(None)` when there is no `-c`, `Err` for an option
/// this parser does not know.
fn shell_script(rest: &[String]) -> Result<Option<&String>, ()> {
    let (mut has_c, mut j) = (false, 0);
    while let Some(w) = rest.get(j) {
        if w == "--" || w == "-" {
            j += 1;
            break;
        }
        if w.starts_with("--") {
            match w.as_str() {
                "--norc" | "--noprofile" | "--login" | "--posix" | "--restricted" | "--verbose" | "--noediting" => j += 1,
                "--rcfile" | "--init-file" => j += 2,
                _ => return Err(()),
            }
            continue;
        }
        if !(w.len() > 1 && w.starts_with(['-', '+'])) {
            break;
        }
        let mut step = 1;
        for c in w[1..].chars() {
            match c {
                'c' => has_c = true,
                'o' | 'O' => step = 2,
                c if c.is_ascii_alphabetic() => {}
                _ => return Err(()),
            }
        }
        j += step;
    }
    Ok(if has_c { rest.get(j) } else { None })
}

/// When `word` (raw) is a redirection, how many words it takes: 1 with its target attached (`>/dev/null`, `2>&1`,
/// `<<EOF`), 2 when the target is the next word (`2> err.log`).
fn redirection_words(word: &str) -> Option<usize> {
    let rest = word.trim_start_matches(|c: char| c.is_ascii_digit());
    let rest = rest.strip_prefix('&').filter(|r| r.starts_with('>')).unwrap_or(rest);
    if !rest.starts_with(['<', '>']) {
        return None;
    }
    let target = rest.trim_start_matches(['<', '>', '&', '|', '-']);
    Some(if target.is_empty() && !rest.ends_with('-') { 2 } else { 1 })
}

fn dirs_of(words: &[String]) -> Vec<String> {
    if matches!(words.first().map(String::as_str), Some("cd" | "pushd" | "popd" | "chdir")) {
        words.get(1).filter(|a| !a.starts_with('-')).cloned().into_iter().collect()
    } else {
        vec![]
    }
}

/// How deep `sh -c`, `eval` and `find -exec` are followed before the command is opaque.
const MAX_DEPTH: usize = 4;

/// The command that really runs for one simple command (`pairs` = its `(raw, unquoted)` words after leading
/// assignments), with wrappers and keywords removed and the program name normalized. `None` when it is not known,
/// in which case `opaque` is set, or when there is no command. Commands it runs in turn (`sh -c`, `eval`, `find
/// -exec`) are appended to `nested`. The returned bool says whether its text can differ from the raw text in a way
/// that matters to a rule (a word in the program/subcommand prefix or an option was quoted, or something was removed).
fn expand(pairs: &[(String, String)], depth: usize, opaque: &mut bool, nested: &mut Vec<Command>) -> Option<(Command, bool)> {
    if depth > MAX_DEPTH {
        *opaque = true;
        return None;
    }
    let vals: Vec<String> = pairs.iter().map(|(_, v)| v.clone()).collect();
    let (mut i, mut changed) = (0, false);
    loop {
        loop {
            if vals.get(i).is_some_and(|v| RESERVED.contains(&v.as_str()) && pairs[i].0 == *v) {
                i += 1;
            } else if let Some(n) = pairs.get(i).and_then(|(raw, _)| redirection_words(raw)) {
                i += n; // `>/dev/null rm x` runs `rm x`
            } else {
                break;
            }
            changed = true;
        }
        let prog_raw = &pairs.get(i)?.0;
        if !matches!(prog_raw.as_str(), "[" | "[[") && prog_raw.contains(['$', '*', '?', '[', '{', '(']) {
            // The shell expands this into a program name this parser cannot know (`$x`, `/bin/r?`, `{rm,-rf,x}`).
            *opaque = true;
            return None;
        }
        let name = program_name(&vals[i]);
        if matches!(name, "function" | "select" | "coproc") {
            *opaque = true;
            return None;
        }
        if !is_wrapper(name) {
            break;
        }
        let Some((k, risky)) = wrapper_end(name, &vals[i + 1..]) else {
            *opaque = true;
            return None;
        };
        *opaque |= risky;
        i += 1 + k;
        changed = true;
        if i >= vals.len() {
            return None;
        }
    }
    let name = program_name(&vals[i]).to_string();
    // A path outside the working directory names the program rules know (`/bin/rm` is `rm`); `./build.sh` and
    // `target/debug/x` are project files and keep their own names.
    let path = &vals[i];
    let leaves_cwd = path.starts_with(['/', '~']) || path.split('/').any(|p| p == "..");
    let name = if !path.contains('/') || leaves_cwd { name } else { path.clone() };
    changed |= name != pairs[i].0;
    let rest = &vals[i + 1..];
    let mut words = vec![name.clone()];
    let mut dirs = vec![];
    if name == "git" {
        let Some((g, git_dirs)) = git_globals(rest) else {
            *opaque = true;
            return None;
        };
        changed |= g > 0;
        dirs = git_dirs;
        words.extend(rest[g..].iter().cloned());
    } else {
        words.extend(rest.iter().cloned());
    }
    if !changed {
        let n = arity(&words);
        changed = pairs[i..].iter().enumerate().any(|(k, (raw, val))| raw != val && (k < n || val.starts_with('-')));
    }
    dirs.extend(dirs_of(&words));
    let (read, secret) = read_paths(pairs, i, &name);
    dirs.extend(read);
    *opaque |= secret;

    let follow = |script: &str, opaque: &mut bool, nested: &mut Vec<Command>| {
        let inner = parse_at(script, depth + 1);
        *opaque |= inner.opaque;
        nested.extend(inner.commands);
    };
    let arithmetic = ["-eq", "-ne", "-lt", "-le", "-gt", "-ge"];
    match name.as_str() {
        "let" => *opaque = true,
        // `export GIT_EXTERNAL_DIFF=x` changes later commands just as `GIT_EXTERNAL_DIFF=x` does; an alias or a `hash
        // -p` entry changes which program a later name runs, and `enable -f` loads a builtin from a library.
        "declare" | "typeset" | "local" | "export" | "readonly" if rest.iter().any(|w| is_assignment(w) && is_risky_assignment(w)) => *opaque = true,
        "alias" if rest.iter().any(|w| w.contains('=')) => *opaque = true,
        "hash" | "enable" if rest.iter().any(|w| w.starts_with('-') && w.contains(['p', 'f'])) => *opaque = true,
        // A file named like an option (`--pre=sh`, which a cloned repository can contain) becomes one when an
        // unquoted glob or a variable expands to it, and rg's `--pre` runs a program on every file it searches.
        "rg" if pairs[i + 1..].iter().take_while(|(_, v)| v != "--").any(|(raw, _)| raw.starts_with(['*', '?', '[', '{', '$']) || raw.starts_with("\"$")) => *opaque = true,
        "[[" if rest.iter().any(|w| arithmetic.contains(&w.as_str())) => *opaque = true,
        "declare" | "typeset" | "local" | "export" | "readonly" if rest.iter().any(|w| w.starts_with('-') && w.contains('i')) => *opaque = true,
        "eval" => follow(&rest.join(" "), opaque, nested),
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "mksh" => match shell_script(rest) {
            Ok(Some(script)) => follow(script, opaque, nested),
            Ok(None) => {}
            Err(()) => *opaque = true,
        },
        "fish" | "csh" | "tcsh" if rest.iter().any(|w| (w.starts_with('-') && !w.starts_with("--") && w.contains('c')) || w == "--command") => *opaque = true,
        "find" => {
            let off = i + 1;
            let mut k = 0;
            while k < rest.len() {
                if matches!(rest[k].as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
                    let end = (k + 1..rest.len()).find(|&e| rest[e] == ";" || rest[e] == "+").unwrap_or(rest.len());
                    let mut more = vec![];
                    if let Some((c, _)) = expand(&pairs[off + k + 1..off + end], depth + 1, opaque, &mut more) {
                        nested.push(c);
                    }
                    nested.extend(more);
                    k = end;
                }
                k += 1;
            }
        }
        _ => {}
    }
    Some((Command { text: words.join(" "), words, dirs }, changed))
}

fn is_assignment(w: &str) -> bool {
    match w.split_once('=') {
        Some((k, _)) => !k.is_empty() && k.chars().enumerate().all(|(i, c)| c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())),
        None => false,
    }
}

/// Variables that change which executable, library, startup file, config file or helper program runs. Cargo reads
/// its whole config from `CARGO_*` (`CARGO_BUILD_RUSTC_WRAPPER`, `CARGO_TARGET_<triple>_RUNNER`...), so only the
/// settings that cannot run anything are left out.
fn is_risky_assignment(w: &str) -> bool {
    let k = w.split_once('=').map_or(w, |(k, _)| k);
    const CARGO_SAFE: &[&str] = &["CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "CARGO_BUILD_JOBS", "CARGO_NET_OFFLINE", "CARGO_LOG"];
    k.starts_with("LD_") || k.starts_with("DYLD_") || k.starts_with("BASH_FUNC_") || k.starts_with("GIT_") || k.starts_with("NPM_CONFIG_")
        || (k.starts_with("CARGO_") && !CARGO_SAFE.contains(&k) && !k.starts_with("CARGO_TERM_"))
        || matches!(
            k,
            "PATH" | "BASH_ENV" | "ENV" | "IFS" | "SHELLOPTS" | "BASHOPTS" | "PS4" | "PROMPT_COMMAND" | "CDPATH" | "GLOBIGNORE"
                | "ZDOTDIR" | "PYTHONPATH" | "PYTHONSTARTUP" | "PYTHONHOME" | "NODE_OPTIONS" | "NODE_PATH" | "RUBYOPT" | "RUBYLIB"
                | "PERL5OPT" | "PERL5LIB" | "PERL5DB" | "RUSTC" | "RUSTC_WRAPPER" | "RUSTC_WORKSPACE_WRAPPER" | "RUSTFLAGS"
                | "RUSTDOC" | "RUSTDOCFLAGS" | "PAGER" | "MANPAGER" | "MANOPT" | "EDITOR" | "VISUAL" | "LESS" | "LESSOPEN"
                | "LESSCLOSE" | "SSH_ASKPASS" | "SUDO_ASKPASS" | "HOME" | "XDG_CONFIG_HOME" | "SHELL" | "RIPGREP_CONFIG_PATH"
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

/// `w` is the long option `full` (with or without `=value`) or an abbreviation of it at least `min` characters long:
/// GNU getopt and git take any unambiguous prefix (`sort --o=x` is `sort --output=x`).
fn long_opt(w: &str, full: &str, min: usize) -> bool {
    let key = w.split_once('=').map_or(w, |(k, _)| k);
    key.starts_with("--") && key.len() >= min && full.starts_with(key)
}

/// The words before a `--`, after which nothing is an option.
fn options_part(args: &[String]) -> &[String] {
    &args[..args.iter().position(|w| w == "--").unwrap_or(args.len())]
}

/// A cluster of short options (`-no`) that has one of `letters` before any of `stop`, which takes the rest of the
/// word as its argument.
fn short_has(w: &str, letters: &str, stop: &str) -> bool {
    let Some(cluster) = w.strip_prefix('-').filter(|c| !c.is_empty() && !c.starts_with('-')) else { return false };
    for c in cluster.chars() {
        if letters.contains(c) {
            return true;
        }
        if stop.contains(c) {
            return false;
        }
    }
    false
}

/// Options and scripts that make a command that looks read-only run another program or write a file that the command
/// line does not show as a write (issue #117): `git log --output=~/.zshrc`, `git diff --ext-diff`, `git config`
/// writes, `rg --pre sh`, `cargo test --config 'target.x.runner="..."'`, `sort -o`, `sed 'w file'` and `sed 'e cmd'`,
/// awk's `system()`, `print > "file"` and pipes, `find -fprint`, `man -P`. Such a command is opaque, like `sed -i`.
fn runs_or_writes_through_options(words: &[String]) -> bool {
    let Some(first) = words.first() else { return false };
    let args = &words[1..];
    let opts = options_part(args);
    let any = |full: &str, min: usize| opts.iter().any(|w| long_opt(w, full, min));
    match program_name(first) {
        "git" => git_runs_or_writes(args),
        "rg" => opts.iter().any(|w| matches!(w.split_once('=').map_or(w.as_str(), |(k, _)| k), "--pre" | "--pre-glob" | "--hostname-bin")),
        // clap does not take abbreviations.
        "cargo" => opts.iter().any(|w| w == "--config" || w.starts_with("--config=")),
        "sort" | "gsort" => any("--output", 3) || any("--compress-program", 4) || opts.iter().any(|w| short_has(w, "o", "ktST")),
        "sed" | "gsed" => sed_runs_or_writes(args),
        "awk" | "gawk" | "mawk" | "nawk" => awk_runs_or_writes(args),
        "find" | "gfind" => args.iter().any(|w| matches!(w.as_str(), "-fprint" | "-fprint0" | "-fprintf" | "-fls")),
        "man" => any("--pager", 4) || any("--html", 4) || opts.iter().any(|w| short_has(w, "PH", "")),
        "sdiff" => any("--output", 3) || any("--diff-program", 4) || opts.iter().any(|w| short_has(w, "o", "w")),
        _ => false,
    }
}

/// git subcommand options that write a file (`--output` of diff, log, show..., `archive -o`) or run a program
/// (`--ext-diff` runs `diff.external`, `grep -O` a pager, `rebase -x`, `--upload-pack`/`--receive-pack`/`--exec` a
/// helper), and `git config` writes, which can set `core.fsmonitor`, `diff.external` or an alias that a later `git
/// status` or `git diff` runs.
fn git_runs_or_writes(args: &[String]) -> bool {
    // `None`: options that run programs or that the parser does not know, which `expand` already made opaque.
    let Some((g, _)) = git_globals(args) else { return false };
    let Some(sub) = args.get(g) else { return false };
    let rest = &args[g + 1..];
    let opts = options_part(rest);
    let any = |full: &str, min: usize| opts.iter().any(|w| long_opt(w, full, min));
    any("--output", 4)
        || any("--ext-diff", 5)
        || any("--upload-pack", 4)
        || any("--receive-pack", 4)
        || opts.iter().any(|w| w == "--exec" || w.starts_with("--exec="))
        || (sub == "grep" && (any("--open-files-in-pager", 4) || opts.iter().any(|w| short_has(w, "O", "efABCm"))))
        || (sub == "rebase" && opts.iter().any(|w| short_has(w, "x", "sXCS")))
        || (sub == "archive" && opts.iter().any(|w| short_has(w, "o", "")))
        || (sub == "config" && git_config_writes(rest))
}

/// `git config` that changes a config file rather than reading it: a name and a value, `set`, `unset`, `edit`..., or an
/// option other than the reading ones.
fn git_config_writes(args: &[String]) -> bool {
    const READ: &[&str] = &["--get", "--get-all", "--get-regexp", "--get-urlmatch", "--get-color", "--get-colorbool", "--list", "-l"];
    const FLAGS: &[&str] = &[
        "--global", "--system", "--local", "--worktree", "--includes", "--no-includes", "--show-origin", "--show-scope", "--name-only", "--null",
        "-z", "--bool", "--int", "--bool-or-int", "--path", "--expiry-date", "--all",
    ];
    const ARGS: &[&str] = &["--file", "-f", "--blob", "--type", "--default"];
    if args.iter().any(|w| READ.contains(&w.as_str())) {
        return false;
    }
    let mut positional = vec![];
    let mut i = 0;
    while let Some(w) = args.get(i) {
        i += 1;
        if !w.starts_with('-') {
            positional.push(w.as_str());
            continue;
        }
        let key = w.split_once('=').map_or(w.as_str(), |(k, _)| k);
        if ARGS.contains(&key) {
            i += usize::from(!w.contains('='));
        } else if !FLAGS.contains(&key) {
            return true;
        }
    }
    match positional.first() {
        Some(&"get" | &"list") => false,
        Some(&"set" | &"unset" | &"edit" | &"rename-section" | &"remove-section") => true,
        _ => positional.len() > 1,
    }
}

/// sed scripts (`-e`, `--expression` or the first operand) that write a file (`w`, `W`, `s///w`) or run a command
/// (GNU `e`, `s///e`), and in-place edits written in ways `is_in_place_writer` does not see (`-ni`, `--in-place=.bak`).
/// A script read from a file (`-f`) is not known, so it counts too; GNU `--sandbox` turns all of them off.
fn sed_runs_or_writes(args: &[String]) -> bool {
    if options_part(args).iter().any(|w| long_opt(w, "--sandbox", 4)) {
        return false;
    }
    let (mut scripts, mut explicit, mut first) = (vec![], false, None);
    let mut i = 0;
    while let Some(w) = args.get(i) {
        i += 1;
        if w == "--" {
            first = first.or(args.get(i));
            break;
        }
        if long_opt(w, "--file", 4) || long_opt(w, "--in-place", 4) {
            return true;
        }
        if long_opt(w, "--expression", 3) {
            explicit = true;
            match w.split_once('=') {
                Some((_, s)) => scripts.push(s.to_string()),
                None => {
                    scripts.extend(args.get(i).cloned());
                    i += 1;
                }
            }
            continue;
        }
        if w.starts_with("--") {
            i += usize::from(long_opt(w, "--line-length", 3) && !w.contains('='));
            continue;
        }
        if let Some(cluster) = w.strip_prefix('-').filter(|c| !c.is_empty()) {
            for (k, c) in cluster.char_indices() {
                let rest = &cluster[k + c.len_utf8()..];
                match c {
                    'f' | 'i' => return true,
                    'e' => {
                        explicit = true;
                        if rest.is_empty() {
                            scripts.extend(args.get(i).cloned());
                            i += 1;
                        } else {
                            scripts.push(rest.to_string());
                        }
                        break;
                    }
                    'l' => {
                        i += usize::from(rest.is_empty());
                        break;
                    }
                    _ => {}
                }
            }
            continue;
        }
        first = first.or(Some(w));
    }
    if !explicit {
        scripts.extend(first.cloned());
    }
    scripts.iter().any(|s| sed_script_runs_or_writes(s))
}

/// Past the closing `d` of a delimited part (regex, replacement) that starts at `cs[*i]`; false when it is not closed.
fn skip_delimited(cs: &[char], i: &mut usize, d: char) -> bool {
    while let Some(&c) = cs.get(*i) {
        *i += 1;
        if c == '\\' {
            *i += 1;
        } else if c == d {
            return true;
        }
    }
    false
}

/// Walks a sed script command by command: addresses, then the command letter and what belongs to it. Anything it
/// does not recognize counts as running or writing (a wrong guess only asks).
fn sed_script_runs_or_writes(script: &str) -> bool {
    let cs: Vec<char> = script.chars().collect();
    let skip_to = |i: &mut usize, end: &[char]| {
        while cs.get(*i).is_some_and(|c| !end.contains(c)) {
            *i += 1;
        }
    };
    let mut i = 0;
    loop {
        while cs.get(i).is_some_and(|c| c.is_whitespace() || *c == ';') {
            i += 1;
        }
        let Some(&start) = cs.get(i) else { return false };
        let mut c = start;
        // Addresses: line numbers, `$`, `/re/`, `\cREc`, ranges, steps, regex flags and negation.
        loop {
            match c {
                '0'..='9' | '$' | ',' | '~' | '+' | '!' | 'I' | 'M' | ' ' | '\t' => i += 1,
                '/' => {
                    i += 1;
                    if !skip_delimited(&cs, &mut i, '/') {
                        return true;
                    }
                }
                '\\' => {
                    let Some(&d) = cs.get(i + 1) else { return true };
                    i += 2;
                    if !skip_delimited(&cs, &mut i, d) {
                        return true;
                    }
                }
                _ => break,
            }
            let Some(&next) = cs.get(i) else { return false };
            c = next;
        }
        i += 1;
        match c {
            'w' | 'W' | 'e' => return true,
            's' | 'y' => {
                let Some(&d) = cs.get(i) else { return true };
                i += 1;
                if matches!(d, '\n' | '\\') || !skip_delimited(&cs, &mut i, d) || !skip_delimited(&cs, &mut i, d) {
                    return true;
                }
                while let Some(&flag) = cs.get(i).filter(|f| !matches!(f, ';' | '\n' | '}')) {
                    if c == 's' && matches!(flag, 'w' | 'e') {
                        return true;
                    }
                    i += 1;
                }
            }
            // Text to the end of the line; a backslash at the end continues it.
            'a' | 'i' | 'c' => loop {
                skip_to(&mut i, &['\n']);
                if i < cs.len() && i > 0 && cs[i - 1] == '\\' {
                    i += 1;
                    continue;
                }
                break;
            },
            'r' | 'R' | '#' => skip_to(&mut i, &['\n']),
            'b' | 't' | 'T' | ':' | 'v' => skip_to(&mut i, &[';', '\n', '}']),
            'q' | 'Q' | 'l' | 'L' => {
                while cs.get(i).is_some_and(|c| c.is_ascii_digit() || *c == ' ') {
                    i += 1;
                }
            }
            '{' | '}' | '=' | 'd' | 'D' | 'g' | 'G' | 'h' | 'H' | 'n' | 'N' | 'p' | 'P' | 'x' | 'z' | 'F' => {}
            _ => return true,
        }
    }
}

/// awk programs that run commands or write files, and options other than `-F`, `-v` and gawk's `-e`/`--source`
/// (`-f progfile`, gawk's `--load`, `-o file`, mawk's `-W exec`...), which count too.
fn awk_runs_or_writes(args: &[String]) -> bool {
    let (mut programs, mut explicit) = (vec![], false);
    let mut i = 0;
    while let Some(w) = args.get(i) {
        i += 1;
        if w == "--" {
            programs.extend(args.get(i).filter(|_| !explicit).cloned());
            break;
        }
        if w == "-" || !w.starts_with('-') {
            if !explicit {
                programs.push(w.clone());
            }
            break;
        }
        let (key, inline) = w.split_once('=').map_or((w.as_str(), None), |(k, v)| (k, Some(v)));
        match key {
            "--field-separator" | "--assign" => i += usize::from(inline.is_none()),
            "--source" => {
                explicit = true;
                programs.extend(inline.map(String::from).or_else(|| args.get(i).cloned()));
                i += usize::from(inline.is_none());
            }
            _ if w.starts_with("-F") || w.starts_with("-v") => i += usize::from(w.len() == 2),
            "-e" => {
                explicit = true;
                programs.extend(args.get(i).cloned());
                i += 1;
            }
            _ => return true,
        }
    }
    programs.iter().any(|p| awk_program_runs_or_writes(p))
}

/// `system()`, a pipe to or from a command (`print | "sh"`, `"cmd" | getline`), gawk's `@load`, `@include` and
/// indirect calls, and `>` in a `print` or `printf` statement outside parentheses, which writes to a file.
fn awk_program_runs_or_writes(p: &str) -> bool {
    let cs: Vec<char> = p.chars().collect();
    let skip_until = |i: &mut usize, end: char| {
        *i += 1;
        while cs.get(*i).is_some_and(|&c| c != end) {
            if cs[*i] == '\\' {
                *i += 1;
            }
            *i += 1;
        }
    };
    let (mut i, mut in_print, mut depth, mut prev) = (0, false, 0usize, None::<char>);
    while let Some(&c) = cs.get(i) {
        match c {
            '"' => skip_until(&mut i, '"'),
            // A regex literal (where a division could not stand); a `|` or `>` in it is not an operator.
            '/' if prev.map_or(true, |p| "(,~!{};&|=".contains(p)) => skip_until(&mut i, '/'),
            '#' => {
                while cs.get(i).is_some_and(|&c| c != '\n') {
                    i += 1;
                }
                continue;
            }
            '|' if cs.get(i + 1) == Some(&'|') => i += 1,
            '|' | '@' => return true,
            '>' if in_print && depth == 0 => return true,
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ';' | '\n' | '{' | '}' => (in_print, depth) = (false, 0),
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while cs.get(i + 1).is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_') {
                    i += 1;
                }
                match cs[start..=i].iter().collect::<String>().as_str() {
                    "print" | "printf" => (in_print, depth) = (true, 0),
                    "system" => return true,
                    _ => {}
                }
            }
            _ => {}
        }
        if !c.is_whitespace() {
            prev = Some(c);
        }
        i += 1;
    }
    false
}

/// Programs that print the files named as their operands (in whole or in part); `grep`, `egrep`, `fgrep` and `rg`
/// take a pattern first.
const READERS: &[&str] = &["cat", "tac", "head", "tail", "less", "more", "nl", "od", "xxd", "hexdump", "strings", "base64", "grep", "egrep", "fgrep", "rg"];

/// The paths a command (`pairs`, its `(raw, unquoted)` words, with the program `name` at `prog`) reads, for the
/// external-directory check: input redirections and the operands of `READERS`, with a leading `$HOME` written as
/// `~`. Also whether one of them is a `.env` file, which the default rules ask for before the read tool reads it (a
/// glob such as `.e*` may be one).
fn read_paths(pairs: &[(String, String)], prog: usize, name: &str) -> (Vec<String>, bool) {
    let mut paths = vec![];
    // `< file`, `<file`, `0<file`, `<>file` anywhere in the command (`< file cat` too); not heredocs, here-strings or
    // `<&3`. Only unquoted operators count: `grep '<' f` has none.
    let mut k = 0;
    while let Some((raw, val)) = pairs.get(k) {
        k += 1;
        let op = raw.trim_start_matches(|c: char| c.is_ascii_digit());
        let Some(target) = op.strip_prefix("<>").or_else(|| op.strip_prefix('<')) else { continue };
        if target.is_empty() {
            paths.extend(pairs.get(k).map(|(_, v)| v.clone()));
            k += 1;
        } else if !target.starts_with(['<', '&', '(']) {
            let v = val.trim_start_matches(|c: char| c.is_ascii_digit());
            paths.extend(v.strip_prefix("<>").or_else(|| v.strip_prefix('<')).map(String::from));
        }
    }
    if READERS.contains(&name) {
        let args = &pairs[(prog + 1).min(pairs.len())..];
        let vals: Vec<String> = args.iter().map(|(_, v)| v.clone()).collect();
        let pattern_first = matches!(name, "grep" | "egrep" | "fgrep" | "rg")
            && !options_part(&vals).iter().any(|w| short_has(w, "ef", "") || long_opt(w, "--regexp", 5) || long_opt(w, "--file", 4));
        let (mut operands, mut skipped) = (false, !pattern_first);
        let mut j = 0;
        while let Some((raw, w)) = args.get(j) {
            j += 1;
            // Redirections were taken above (a `>` target is a write, which makes the command opaque anyway).
            if let Some(n) = redirection_words(raw) {
                j += n - 1;
                continue;
            }
            if !operands && w == "--" {
                operands = true;
                continue;
            }
            // Options are skipped; an option's argument is taken as an operand, which can only add a check.
            if !operands && w.starts_with('-') && w != "-" {
                continue;
            }
            if !skipped {
                skipped = true;
                continue;
            }
            paths.push(w.clone());
        }
    }
    let paths: Vec<String> = paths
        .into_iter()
        .filter(|p| !p.is_empty() && p != "-" && !p.starts_with("/dev/fd/") && !matches!(p.as_str(), "/dev/null" | "/dev/stdin" | "/dev/zero" | "/dev/random" | "/dev/urandom"))
        .map(|p| match p.strip_prefix("${HOME}").or_else(|| p.strip_prefix("$HOME")) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
            _ => p,
        })
        .collect();
    let secret = paths.iter().any(|p| {
        let base = p.rsplit('/').next().unwrap_or(p);
        let env = |n: &str| (crate::wildcard::matches(n, "*.env") || crate::wildcard::matches(n, "*.env.*")) && !crate::wildcard::matches(n, "*.env.example");
        env(base) || (base.starts_with(['.', '{']) && base.contains(['*', '?', '[', '{']))
    });
    (paths, secret)
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
    parse_at(input, 0)
}

/// End of `((...))`, `$((...))` or `$[...]` whose first `open` is at `cs[start]`: the index after the bracket that
/// closes it, or the end of input.
fn arithmetic_end(cs: &[char], start: usize, open: char, close: char) -> usize {
    let (mut depth, mut quote) = (0usize, None);
    for (k, &c) in cs.iter().enumerate().skip(start) {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == open => depth += 1,
            None if c == close => {
                depth -= 1;
                if depth == 0 {
                    return k + 1;
                }
            }
            None => {}
        }
    }
    cs.len()
}

type Words = Vec<(String, String)>;

fn parse_at(input: &str, depth: usize) -> Parsed {
    let mut parsed = Parsed::default();
    let mut heredocs: Vec<Heredoc> = vec![];
    let cs: Vec<char> = input.chars().collect();
    let mut raw_words: Words = vec![]; // (raw, unquoted)
    // Per command in `parsed.commands`: its words after leading assignments, and the length of its text before a
    // heredoc body was appended.
    let mut sources: Vec<(Words, usize)> = vec![];
    let (mut raw, mut val) = (String::new(), String::new());
    let mut in_word = false;
    let (mut sq, mut dq) = (false, false);
    let mut i = 0;

    let flush_word = |raw: &mut String, val: &mut String, in_word: &mut bool, words: &mut Words| {
        if *in_word {
            words.push((std::mem::take(raw), std::mem::take(val)));
            *in_word = false;
        }
    };
    let finish = |words: &mut Words, parsed: &mut Parsed, sources: &mut Vec<(Words, usize)>| {
        let skip = words.iter().take_while(|(_, v)| is_assignment(v)).count();
        let risky = words.iter().take(skip).any(|(_, v)| is_risky_assignment(v));
        // A literal `$(...)` or backtick stored in a variable runs when bash evaluates the variable in arithmetic
        // (`x='a[$(id)]'; [[ $x -eq 1 ]]`).
        let stored_code = words.iter().take(skip).any(|(_, v)| v.contains("$(") || v.contains('`'));
        if risky || stored_code {
            parsed.opaque = true;
        }
        // Risky assignments stay in the text; harmless ones are dropped as before.
        let kept = if risky { 0 } else { skip };
        let has_redirect = has_output_redirection(words);
        let all: Words = std::mem::take(words);
        let text = all.iter().skip(kept).map(|(r, _)| r.as_str()).collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            return;
        }
        let source: Words = all.iter().skip(skip).cloned().collect();
        let mut w: Vec<String> = source.iter().map(|(_, v)| v.clone()).collect();
        if w.is_empty() {
            // Assignment-only command (`PATH=./x`): it still changes later commands.
            w = text.split(' ').map(String::from).collect();
        }
        if has_redirect {
            parsed.opaque = true;
        }
        sources.push((source, text.len()));
        parsed.commands.push(Command { text, words: w, dirs: vec![] });
    };

    while i < cs.len() {
        let c = cs[i];
        if !sq && c == '$' && cs.get(i + 1) == Some(&'{') && cs.get(i + 2) == Some(&'(') {
            // zsh parameter flags: `${(e)x}` evaluates the value as shell code.
            parsed.opaque = true;
        }
        if sq {
            raw.push(c);
            if c == '\'' { sq = false } else { val.push(c) }
        } else if dq {
            match c {
                '"' => { dq = false; raw.push(c); }
                // A backslash-newline is removed: the line continues.
                '\\' if cs.get(i + 1) == Some(&'\n') => i += 1,
                '\\' if i + 1 < cs.len() => { raw.push(c); raw.push(cs[i + 1]); val.push(cs[i + 1]); i += 1; }
                '`' => { parsed.opaque = true; raw.push(c); val.push(c); }
                '$' if matches!(cs.get(i + 1), Some('(' | '[')) => { parsed.opaque = true; raw.push(c); val.push(c); }
                _ => { raw.push(c); val.push(c); }
            }
        } else {
            let arithmetic = (c == '$' && cs.get(i + 1) == Some(&'(') && cs.get(i + 2) == Some(&'('))
                || (c == '(' && cs.get(i + 1) == Some(&'(') && !in_word)
                || (c == '$' && cs.get(i + 1) == Some(&'['));
            if arithmetic {
                // `<<` and `;` inside are operators, not a heredoc or a separator, so the text is taken as one piece
                // up to its closing bracket and the lines after it stay commands. bash also evaluates variables'
                // contents there as arithmetic (which can run `$(...)` stored in them), so it asks.
                parsed.opaque = true;
                in_word = true;
                let (open, close) = if cs.get(i + 1) == Some(&'[') { ('[', ']') } else { ('(', ')') };
                let start = if c == '$' { i + 1 } else { i };
                let end = arithmetic_end(&cs, start, open, close);
                let piece: String = cs[i..end].iter().collect();
                raw.push_str(&piece);
                val.push_str(&piece);
                i = end;
                continue;
            }
            match c {
                '\'' => { sq = true; in_word = true; raw.push(c); }
                '"' => { dq = true; in_word = true; raw.push(c); }
                // A backslash-newline is removed before the line is split into words: `r\<newline>m` is `rm`.
                '\\' if cs.get(i + 1) == Some(&'\n') => i += 1,
                '\\' if i + 1 < cs.len() => { in_word = true; raw.push(c); raw.push(cs[i + 1]); val.push(cs[i + 1]); i += 1; }
                // A comment runs to the end of the line; a `<<` or quote in it is not shell syntax.
                '#' if !in_word => {
                    i = (i..cs.len()).find(|&k| cs[k] == '\n').unwrap_or(cs.len());
                    continue;
                }
                '`' => { parsed.opaque = true; in_word = true; raw.push(c); val.push(c); }
                '$' if cs.get(i + 1) == Some(&'(') => { parsed.opaque = true; in_word = true; raw.push(c); val.push(c); }
                '<' | '>' if cs.get(i + 1) == Some(&'(') => { parsed.opaque = true; in_word = true; raw.push(c); val.push(c); }
                // A subshell: the commands inside are commands like any other.
                '(' if !in_word && raw_words.is_empty() => {}
                // Anywhere else `(` is something this parser does not model: zsh glob qualifiers (`*(e:cmd:)`), zsh
                // `=(cmd)`, bash extglob, arrays, function definitions.
                '(' => { parsed.opaque = true; in_word = true; raw.push(c); val.push(c); }
                ')' => {
                    flush_word(&mut raw, &mut val, &mut in_word, &mut raw_words);
                    finish(&mut raw_words, &mut parsed, &mut sources);
                }
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
                    finish(&mut raw_words, &mut parsed, &mut sources);
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
                        finish(&mut raw_words, &mut parsed, &mut sources);
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
    finish(&mut raw_words, &mut parsed, &mut sources);
    if !heredocs.is_empty() {
        // `cat <<EOF` with no body at all: the shell reads to end of input. Nothing to guess from, so ask.
        parsed.opaque = true;
    }
    if input.contains("@P}") {
        // bash `${x@P}` expands the value like a prompt, which runs `$(...)` in it.
        parsed.opaque = true;
    }

    // Follow each command to what really runs (module docs).
    let mut commands = Vec::with_capacity(parsed.commands.len());
    for (mut command, (source, base_len)) in std::mem::take(&mut parsed.commands).into_iter().zip(sources) {
        let mut nested = vec![];
        let real = if source.is_empty() { None } else { expand(&source, depth, &mut parsed.opaque, &mut nested) };
        match real {
            Some((mut real, changed)) => {
                command.dirs = std::mem::take(&mut real.dirs);
                // A heredoc body is input to the command that really runs.
                real.text.push_str(&command.text[base_len..]);
                let differs = changed && real.text != command.text;
                commands.push(command);
                if differs {
                    commands.push(real);
                }
            }
            None => {
                command.dirs = dirs_of(&command.words);
                commands.push(command);
            }
        }
        commands.extend(nested);
    }
    parsed.commands = commands;
    if parsed.commands.iter().any(|c| python_writes(c) || is_in_place_writer(&c.words) || runs_or_writes_through_options(&c.words)) {
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
        // The body is a program, so "always" does not save `osascript *` (that would cover any `-e 'do shell script
        // ...'`, #118); a rule the user wrote still covers it.
        assert_eq!(save_pattern(&p.commands[0].words), "osascript <<EOF 2>&1");
        assert!(crate::wildcard::matches(&p.commands[0].text, "osascript *"), "a configured `osascript *` covers it");

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
        assert_eq!(pat("git"), "git", "no subcommand: `git *` would cover every git command");
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

    fn matched(c: &str, rule: &str) -> bool {
        parse(c).commands.iter().any(|x| crate::wildcard::matches(&x.text, rule))
    }

    #[test]
    fn arithmetic_shifts_are_not_heredocs() {
        // Issue #118: bash reads `<<` in arithmetic as a shift and runs the next line; it must stay a command.
        for c in [
            "((x=1<<2))\nrm -rf ~\n2",
            "for ((i=0; i<1<<2; i++)); do echo $i; done\nrm -rf ~\n2",
            "echo $((1<<2))\nrm -rf ~\n2",
            "echo \"$((1<<2))\"\nrm -rf ~\n2",
            "echo $[1<<2]\nrm -rf ~\n2",
        ] {
            let p = parse(c);
            assert!(p.opaque, "{c:?}");
            assert!(p.commands.iter().any(|x| x.text == "rm -rf ~"), "{c:?}: {:?}", p.commands);
        }
        // bash evaluates variables in arithmetic, which runs a `$(...)` stored in them.
        for c in ["let x=1+2", "[[ $x -eq 1 ]]", "x='a[$(id)]'; [[ $x -eq 1 ]]", "declare -i n=$x"] {
            assert!(parse(c).opaque, "{c}");
        }
        assert!(!parse("[[ -f Cargo.toml ]] && cargo test").opaque);
    }

    #[test]
    fn comments_and_line_continuations_are_read_like_the_shell() {
        // A `<<` in a comment is not a heredoc: the next line runs.
        assert!(matched("ls # <<EOF\nrm -rf ~\nEOF", "rm *"));
        assert_eq!(texts("ls -la # list them"), ["ls -la"]);
        // A backslash-newline joins the lines: `r\<newline>m` is `rm`.
        assert_eq!(texts("r\\\nm -rf x"), ["rm -rf x"]);
        assert_eq!(texts("cargo test \\\n  -p moco-core"), ["cargo test -p moco-core"]);
        assert_eq!(texts("echo \"a\\\nb\""), ["echo \"ab\""]);
    }

    #[test]
    fn zsh_and_other_unmodelled_syntax_is_opaque() {
        for c in [
            "cat =(id)",                       // zsh process substitution
            "ls *(e:'touch pwned':)",          // zsh glob qualifier that runs code
            "ls -d *(/)",
            "print ${(e)x}",                   // zsh parameter flag that evaluates
            "echo \"${(e)x}\"",
            "echo ${x@P}",                     // bash prompt expansion
            "ls !(x)",                         // extglob
            "a=(1 2)",
            "f() { rm -rf ~; }; f",
            "function f { id; }",
            "$x -rf ~",                        // the program is not known until the shell expands it
            "/bin/r? -rf x",
            "{rm,-rf,x}",
            "env -S 'rm -rf x'",               // options a wrapper parser does not know
            "xargs --frobnicate rm",
            "git -c alias.st='!rm -rf ~' st",  // git options that run programs
            "git --exec-path=/tmp/evil status",
            "bash -c 'echo $(id)'",
        ] {
            assert!(parse(c).opaque, "{c}");
        }
        // A subshell is a group of ordinary commands.
        let p = parse("(cd sub && ls -la)");
        assert!(!p.opaque);
        assert_eq!(p.commands.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(), ["cd sub", "ls -la"]);
        assert_eq!(p.commands[0].dirs, ["sub"]);
    }

    #[test]
    fn options_that_run_programs_or_write_files_are_opaque() {
        // Issue #117: the careful preset allows several of these programs, so an option or script that turns one into
        // a file write or a program run must ask, and never reach the classifier.
        for c in [
            "git log -1 --format=%B --output=/home/u/.zshrc",
            "git log --outp=x",
            "git diff --output x",
            "git show HEAD '--output=x'",
            "git -C sub show --output=x",
            "git diff --ext-diff",
            "git log -p --ext",
            "git grep -O hi",
            "git grep --open-files-in-pager=vim hi",
            "git rebase -x 'make' main",
            "git fetch --upload-pack='sh -c id' origin",
            "git archive -o /tmp/x HEAD",
            "git config core.fsmonitor 'sh -c id'",
            "git config set alias.st '!id'",
            "git config --add diff.external x",
            "git config --unset core.pager",
            "git config -e",
            "rg --pre sh x",
            "rg --pre=/bin/sh x",
            "rg x --pre-glob '*' --pre cat",
            "rg --hostname-bin=./x x",
            "rg x *",
            "rg x $DIR",
            "rg x \"$DIR\"",
            "cargo test --config 'target.x86_64-apple-darwin.runner=\"sh -c id\"'",
            "cargo --config build.rustc-wrapper=./x build",
            "cargo check --config=./evil.toml",
            "sort -o ~/.zshrc x",
            "sort -no x y",
            "sort --o=x y",
            "sort --compress-program=sh y",
            "sed 's/a/b/w /tmp/x' f",
            "sed -n '1w out' f",
            "sed -e p -e 'W out' f",
            "sed --expression='1e id' f",
            "sed 's/x/id/e' f",
            "sed -f script.sed f",
            "sed -ni 's/a/b/' f",
            "sed --in-place=.bak 's/a/b/' f",
            "sed --line-length 5 'w x' f",
            "awk 'BEGIN{system(\"id\")}'",
            "awk '{print > \"/tmp/x\"}' f",
            "awk '{printf \"%s\", $0 >> \"x\"}' f",
            "awk '{print | \"sh\"}' f",
            "awk 'BEGIN{\"id\" | getline x}'",
            "awk -f prog.awk f",
            "gawk '@load \"x\"; BEGIN{}'",
            "find . -fprint /tmp/x",
            "find . -name x -fprintf out '%p'",
            "man -P 'sh -c id' ls",
            "man --pager=x ls",
            // Variables that make one of them run something else.
            "export GIT_EXTERNAL_DIFF=./x; git diff",
            "export RIPGREP_CONFIG_PATH=./c; rg x",
            "RIPGREP_CONFIG_PATH=./c rg x",
            "CARGO_BUILD_RUSTC_WRAPPER=./x cargo build",
            "XDG_CONFIG_HOME=./x git status",
            "LESS='+!id' less f",
            "alias cat='sh -c id'; cat x",
            "hash -p ./x git; git status",
        ] {
            assert!(parse(c).opaque, "{c}");
        }
        for c in [
            "git log --oneline -20",
            "git log -p -1 --format='%h %s'",
            "git diff --stat HEAD~1",
            "git diff --output-indicator-new=+ x",
            "git diff --no-ext-diff",
            "git show HEAD:src/lib.rs",
            "git grep -n hi",
            "git config user.name",
            "git config --get core.pager",
            "git config --global --list",
            "git config get user.email",
            "git log -- --output=x",
            "rg 'struct Foo' crates/",
            "rg -n --glob '*.rs' unwrap",
            "rg x '*'",
            "rg x -- *",
            "cargo test -p moco-core",
            "cargo test -- --config",
            "CARGO_TARGET_DIR=/tmp/t cargo build",
            "CARGO_TERM_COLOR=always cargo test",
            "sort names.txt",
            "sort -t, -k2 x",
            "sort -to x",
            "sed -n '1,40p' src/lib.rs",
            "sed 's/foo/bar/g' f",
            "sed -e '/^$/d' -e 's|a|b|' f",
            "sed -n '$!N;s/\\n/ /;p' f",
            "sed --sandbox 'w x' f",
            "sed -n '/start/,/end/{/x/p}' f",
            "sed 'y/abc/xyz/' f",
            "sed '1i\\\nheader' f",
            "awk '{print $1}' access.log",
            "awk -F: '$3 > 100 {print $1}' f",
            "awk '{print ($1 > 2)}' f",
            "awk '$1 ~ /a|b/ {n++} END {print n}' f",
            "awk -v n=3 'NR <= n || /x/' f",
            "awk '{printf \"%s > %s\\n\", $1, $2}' f",
            "find . -name '*.rs'",
            "man ls",
            "export FOO=1; ls",
        ] {
            assert!(!parse(c).opaque, "{c}");
        }
    }

    #[test]
    fn files_that_read_commands_read_get_the_external_check() {
        let dirs = |c: &str| parse(c).commands.into_iter().flat_map(|c| c.dirs).collect::<Vec<_>>();
        assert_eq!(dirs("cat ~/.ssh/id_rsa"), ["~/.ssh/id_rsa"]);
        assert_eq!(dirs("head -5 /etc/hosts src/a.rs"), ["/etc/hosts", "src/a.rs"]);
        assert_eq!(dirs("grep -rn KEY ~"), ["~"], "the pattern is not a path");
        assert_eq!(dirs("grep -eKEY -- -x /etc"), ["-x", "/etc"]);
        assert_eq!(dirs("grep '>' /etc/x"), ["/etc/x"], "a quoted `>` is the pattern, not a redirection");
        assert_eq!(dirs("rg TODO /etc"), ["/etc"]);
        assert_eq!(dirs("/bin/cat ../../x"), ["../../x"]);
        assert_eq!(dirs("cat $HOME/.aws/credentials"), ["~/.aws/credentials"]);
        assert_eq!(dirs("wc -c < ~/.ssh/id_rsa"), ["~/.ssh/id_rsa"], "input redirections of any command");
        assert_eq!(dirs("< /etc/passwd tr a b"), ["/etc/passwd"]);
        assert_eq!(dirs("cat /dev/null - x 2>/dev/null"), ["x"]);
        assert!(dirs("ls /etc").is_empty(), "listing is not reading");
        assert!(dirs("cat <<EOF\nx\nEOF").is_empty());
        // `.env` asks before the read tool reads it; a shell command that prints it asks too.
        for c in ["cat .env", "head -3 app/.env.local", "grep KEY .env", "tail x .e*", "cat < .env", "cat {.env,x}", "cat x.env"] {
            assert!(parse(c).opaque, "{c}");
        }
        for c in ["cat .env.example", "cat README.md", "grep -rn env src", "grep .env src"] {
            assert!(!parse(c).opaque, "{c}");
        }
    }

    #[test]
    fn rules_see_the_command_that_really_runs() {
        assert_eq!(texts("'git' push origin"), ["'git' push origin", "git push origin"]);
        assert_eq!(texts("git -C . push"), ["git -C . push", "git push"]);
        assert_eq!(texts("/bin/rm -rf x"), ["/bin/rm -rf x", "rm -rf x"]);
        assert_eq!(texts("env rm x"), ["env rm x", "rm x"]);
        for (c, rule) in [
            ("'git' push", "git push *"),
            ("g\\it push", "git push *"),
            ("git -C . push", "git push *"),
            ("git --no-pager -C sub --git-dir=.git push -f", "git push *"),
            ("/usr/bin/git push", "git push *"),
            ("env rm x", "rm *"),
            ("env -i -u HOME -- FOO=1 rm x", "rm *"),
            ("/bin/rm x", "rm *"),
            ("../../../bin/rm x", "rm *"),
            ("=rm x", "rm *"),
            ("\\rm -rf x", "rm -rf *"),
            ("rm '-rf' x", "rm -rf *"),
            ("nohup nice -n 5 rm x &", "rm *"),
            ("time -p rm x", "rm *"),
            ("timeout -s KILL 5 rm x", "rm *"),
            ("xargs -0 -n1 -I{} rm {}", "rm *"),
            ("command rm x", "rm *"),
            ("builtin exec rm x", "rm *"),
            ("sudo -u root rm x", "rm *"),
            ("caffeinate -i rm x", "rm *"),
            ("noglob rm x", "rm *"),
            ("if true; then rm x; fi", "rm *"),
            ("while true; do rm x; done", "rm *"),
            ("{ rm x; }", "rm *"),
            ("! rm x", "rm *"),
            ("(rm x)", "rm *"),
            (">/dev/null rm x", "rm *"),
            ("2> /dev/null rm x", "rm *"),
            ("bash -c 'git push'", "git push *"),
            ("sh -lc \"rm -rf x\"", "rm -rf *"),
            ("zsh -o pipefail -c 'env rm x'", "rm *"),
            ("eval 'rm x'", "rm *"),
            ("find . -name '*.o' -exec rm {} \\;", "rm *"),
            ("find . -execdir sh -c 'rm x' \\;", "rm *"),
            ("xargs sh -c 'rm x'", "rm *"),
        ] {
            assert!(matched(c, rule), "{c} must match {rule}: {:?}", parse(c).commands);
        }
        // Directories reached through wrappers and git options are checked too.
        let dirs = |c: &str| parse(c).commands.into_iter().flat_map(|c| c.dirs).collect::<Vec<_>>();
        assert_eq!(dirs("git -C /etc status"), ["/etc"]);
        assert_eq!(dirs("builtin cd /tmp && ls"), ["/tmp"]);
        assert_eq!(dirs("bash -c 'cd /etc && ls'"), ["/etc"]);
    }

    #[test]
    fn ordinary_commands_keep_their_resources() {
        for c in [
            "git status",
            "cargo test -p moco-core",
            "ls -la",
            "git commit -m 'fix: it'",
            "grep -rn 'a b' src",
            "./scripts/test.sh --quick",
            "target/debug/moco --version",
            "cat <<'EOF'\nhello\nEOF",
            "FOO=1 cargo test",
            "echo \"a && b\"",
        ] {
            let p = parse(c);
            assert!(!p.opaque, "{c}");
            assert_eq!(p.commands.len(), 1, "{c}: {:?}", p.commands);
        }
        assert_eq!(texts("FOO=1 cargo test"), ["cargo test"]);
    }

    #[test]
    fn always_patterns_never_cover_more_code_than_was_approved() {
        let pat = |c: &str| save_pattern(&parse(c).commands[0].words);
        for (c, want) in [
            ("git status", "git status *"),
            ("cargo test -p x", "cargo test *"),
            ("ls -la", "ls *"),
            ("git -C sub status", "git -C sub status *"),
            ("git --no-pager log -1", "git --no-pager log *"),
            ("git -C sub", "git -C sub"),
            ("python3 x.py --flag", "python3 x.py *"),
            ("python3 -m pytest -q", "python3 -m pytest *"),
            ("python3 -c 'print(1)'", "python3 -c print(1)"),
            ("python3", "python3"),
            ("node -e 'x'", "node -e x"),
            ("osascript -e 'beep'", "osascript -e beep"),
            ("bash run.sh a", "bash run.sh *"),
            ("bash -c 'ls'", "bash -c ls"),
            ("sh", "sh"),
            ("env FOO=1 cargo test", "env FOO=1 cargo test *"),
            ("env", "env"),
            ("env -S 'rm -rf x'", "env -S rm -rf x"),
            ("xargs grep foo", "xargs grep *"),
            ("xargs", "xargs"),
            ("xargs python3 -c 'x'", "xargs python3 -c x"),
            ("timeout 60 cargo test -p x", "timeout 60 cargo test *"),
            ("find . -name '*.rs'", "find . -name ?.rs"),
            ("awk '{print $1}' f", "awk {print $1} f"),
            ("sudo ls", "sudo ls"),
            ("bun -e 'x'", "bun -e x"),
            ("npm -g install x", "npm -g install x"),
            ("then rm x", "then rm *"),
            (">/dev/null ls", ">/dev/null ls *"),
        ] {
            assert_eq!(pat(c), want, "{c}");
        }
        // None of them covers code the user did not see.
        for (approved, other) in [
            ("python3 x.py", "python3 -c import os"),
            ("git -C sub status", "git -C sub push"),
            ("env FOO=1 cargo test", "env FOO=1 sh -c id"),
            ("xargs grep foo", "xargs sh -c id"),
            ("find . -name '*.rs'", "find . -name x -exec sh -c id ; -name y.rs"),
            ("bash run.sh", "bash -c id"),
            ("timeout 60 cargo test", "timeout 60 sh -c id"),
        ] {
            assert!(!crate::wildcard::matches(other, &pat(approved)), "{approved} -> {} covers {other}", pat(approved));
        }
        // The command that really runs is saved by its own pattern.
        assert_eq!(save_pattern(&parse("/bin/rm x").commands[1].words), "rm *");
    }
}
