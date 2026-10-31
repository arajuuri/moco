//! Formatter hooks (docs/08 "Formatters and LSP"): after the model writes, edits or patches a file, the formatter
//! configured for its extension runs on it. A failing formatter never fails the edit; the model is told what happened.
use crate::config::FormatterConfig;
use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct Formatter {
    pub name: String,
    pub command: Vec<String>,
    /// Lowercase, with the leading dot.
    pub extensions: Vec<String>,
    pub environment: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// No formatter covers this file.
    NotApplicable,
    Unchanged,
    Changed { name: String },
    Failed { name: String, message: String },
}

#[derive(Debug, Clone)]
pub struct Formatters {
    list: Vec<Formatter>,
    timeout: Duration,
}

impl Default for Formatters {
    fn default() -> Self {
        Formatters { list: vec![], timeout: Duration::from_secs(30) }
    }
}

const BOM: [u8; 3] = [0xef, 0xbb, 0xbf];

fn norm_ext(e: &str) -> String {
    let e = e.trim().to_lowercase();
    if e.starts_with('.') { e } else { format!(".{e}") }
}

impl Formatters {
    /// Build from config. `allowed` decides per formatter name whether it may run (project-defined ones need approval).
    pub fn from_config(map: &BTreeMap<String, FormatterConfig>, allowed: &dyn Fn(&str) -> bool) -> Formatters {
        let list = map
            .iter()
            .filter(|(name, c)| !c.disabled && !c.command.is_empty() && !c.extensions.is_empty() && allowed(name))
            .map(|(name, c)| Formatter {
                name: name.clone(),
                command: c.command.clone(),
                extensions: c.extensions.iter().map(|e| norm_ext(e)).collect(),
                environment: c.environment.clone(),
            })
            .collect();
        Formatters { list, timeout: Duration::from_secs(30) }
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn names(&self) -> Vec<&str> {
        self.list.iter().map(|f| f.name.as_str()).collect()
    }

    /// The first formatter (in name order) that handles `path`'s extension.
    pub fn for_file(&self, path: &Path) -> Option<&Formatter> {
        let name = path.file_name()?.to_string_lossy().to_lowercase();
        self.list.iter().find(|f| f.extensions.iter().any(|e| name.ends_with(e.as_str()) && name.len() > e.len()))
    }

    /// Format `path` in place (working directory `cwd`). A byte-order mark the file had is put back if the formatter dropped it.
    pub fn run(&self, path: &Path, cwd: &Path) -> Outcome {
        let Some(f) = self.for_file(path) else { return Outcome::NotApplicable };
        let fail = |message: String| Outcome::Failed { name: f.name.clone(), message };
        let Ok(before) = std::fs::read(path) else { return fail("the file could not be read".into()) };
        let file = path.to_string_lossy();
        let mut args: Vec<String> = f.command.iter().map(|a| a.replace("$FILE", &file)).collect();
        if !f.command.iter().any(|a| a.contains("$FILE")) {
            args.push(file.to_string());
        }
        let log = std::env::temp_dir().join(format!("{}.log", crate::id::new_id("fmt")));
        let Ok(out) = std::fs::File::create(&log) else { return fail("no temporary file for the formatter's output".into()) };
        let Ok(err) = out.try_clone() else { return fail("no temporary file for the formatter's output".into()) };
        let child = Command::new(&args[0]).args(&args[1..]).current_dir(cwd).envs(&f.environment).stdin(Stdio::null()).stdout(Stdio::from(out)).stderr(Stdio::from(err)).process_group(0).spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::remove_file(&log);
                return fail(format!("cannot start '{}': {e}", args[0]));
            }
        };
        let pid = child.id();
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) if start.elapsed() >= self.timeout => {
                    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                    let _ = child.wait();
                    break None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break None,
            }
        };
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) }; // anything it left behind
        let output = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(&log);
        let first_lines = |s: &str| s.lines().map(str::trim).filter(|l| !l.is_empty()).take(4).collect::<Vec<_>>().join(" | ");
        match status {
            None => return fail(format!("timed out after {} s", self.timeout.as_secs())),
            Some(s) if !s.success() => {
                let detail = first_lines(&output);
                return fail(format!("exit code {}{}", s.code().map_or("?".to_string(), |c| c.to_string()), if detail.is_empty() { String::new() } else { format!(": {detail}") }));
            }
            Some(_) => {}
        }
        let Ok(mut after) = std::fs::read(path) else { return fail("the file is gone after formatting".into()) };
        if before.starts_with(&BOM) && !after.starts_with(&BOM) {
            let mut with = BOM.to_vec();
            with.extend_from_slice(&after);
            after = with;
            if std::fs::write(path, &after).is_err() {
                return fail("could not restore the byte-order mark".into());
            }
        }
        if after == before { Outcome::Unchanged } else { Outcome::Changed { name: f.name.clone() } }
    }
}

/// The sentence that goes at the end of a tool's output, if the formatter had something to say.
pub fn note(outcome: &Outcome) -> Option<String> {
    match outcome {
        Outcome::Changed { name } => Some(format!("Formatted with {name}: the file now differs from what you wrote, so read it again before editing it further.")),
        Outcome::Failed { name, message } => Some(format!("The formatter {name} failed ({message}); the file was left as written.")),
        Outcome::NotApplicable | Outcome::Unchanged => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// `sh -c` formatters: `$0` is the file. This one upper-cases it in place.
    fn upcase() -> FormatterConfig {
        FormatterConfig { command: vec!["sh".into(), "-c".into(), "tr a-z A-Z < \"$0\" > \"$0.new\" && mv \"$0.new\" \"$0\"".into(), "$FILE".into()], extensions: vec!["txt".into(), ".MD".into()], ..Default::default() }
    }

    /// Formatters for `.txt` and `.md` files that upper-case them (what the tool tests use).
    pub(crate) fn upcase_formatters() -> Formatters {
        Formatters::from_config(&BTreeMap::from([("upcase".to_string(), upcase())]), &|_| true)
    }

    pub(crate) fn failing_formatters() -> Formatters {
        let c = FormatterConfig { command: vec!["sh".into(), "-c".into(), "echo 'parse error at 1:1' >&2; exit 1".into()], extensions: vec!["txt".into()], ..Default::default() };
        Formatters::from_config(&BTreeMap::from([("strict".to_string(), c)]), &|_| true)
    }

    fn setup() -> (std::path::PathBuf, Formatters) {
        let dir = std::env::temp_dir().join(crate::id::new_id("fmt-test"));
        std::fs::create_dir_all(&dir).unwrap();
        (dir, Formatters::from_config(&BTreeMap::from([("upcase".to_string(), upcase())]), &|_| true))
    }

    #[test]
    fn extensions_match_case_insensitively_on_the_file_name() {
        let (_, f) = setup();
        for (name, want) in [("a.txt", true), ("A.TXT", true), ("notes.md", true), ("x.y.txt", true), (".txt", false), ("a.txtx", false), ("txt", false), ("a.rs", false), ("a.md.bak", false)] {
            assert_eq!(f.for_file(Path::new(name)).is_some(), want, "{name}");
        }
        assert_eq!(f.names(), ["upcase"]);
        assert!(Formatters::default().is_empty());
    }

    #[test]
    fn from_config_skips_disabled_incomplete_and_disallowed_entries() {
        let mk = |c: &[&str], e: &[&str], disabled| FormatterConfig { command: c.iter().map(|s| s.to_string()).collect(), extensions: e.iter().map(|s| s.to_string()).collect(), disabled, ..Default::default() };
        let map = BTreeMap::from([
            ("ok".to_string(), mk(&["fmt"], &["a"], false)),
            ("off".to_string(), mk(&["fmt"], &["b"], true)),
            ("nocmd".to_string(), mk(&[], &["c"], false)),
            ("noext".to_string(), mk(&["fmt"], &[], false)),
            ("denied".to_string(), mk(&["fmt"], &["d"], false)),
        ]);
        assert_eq!(Formatters::from_config(&map, &|n| n != "denied").names(), ["ok"]);
    }

    #[test]
    fn changed_unchanged_and_not_applicable() {
        let (dir, f) = setup();
        let p = dir.join("a.txt");
        std::fs::write(&p, "hello").unwrap();
        assert_eq!(f.run(&p, &dir), Outcome::Changed { name: "upcase".into() });
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "HELLO");
        assert_eq!(f.run(&p, &dir), Outcome::Unchanged, "already formatted");
        let other = dir.join("a.rs");
        std::fs::write(&other, "fn x() {}").unwrap();
        assert_eq!(f.run(&other, &dir), Outcome::NotApplicable);
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "fn x() {}");
        assert_eq!(note(&Outcome::Unchanged), None);
        assert!(note(&Outcome::Changed { name: "upcase".into() }).unwrap().contains("read it again"));
    }

    #[test]
    fn arguments_environment_and_working_directory() {
        let dir = std::env::temp_dir().join(crate::id::new_id("fmt-args"));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f.txt");
        std::fs::write(&p, "x").unwrap();
        let write_info = |cmd: Vec<&str>, env: &[(&str, &str)]| {
            let c = FormatterConfig { command: cmd.into_iter().map(String::from).collect(), extensions: vec!["txt".into()], environment: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), ..Default::default() };
            Formatters::from_config(&BTreeMap::from([("t".to_string(), c)]), &|_| true)
        };
        // `$FILE` inside an argument; env and cwd visible to the program.
        let f = write_info(vec!["sh", "-c", "printf '%s|%s|%s' \"$1\" \"$FMT_VAR\" \"$(pwd)\" > \"$2\"", "sh", "--file=$FILE", "$FILE"], &[("FMT_VAR", "v")]);
        assert_eq!(f.run(&p, &dir), Outcome::Changed { name: "t".into() });
        let got = std::fs::read_to_string(&p).unwrap();
        let (arg, rest) = got.split_once('|').unwrap();
        assert_eq!(arg, format!("--file={}", p.display()));
        assert!(rest.starts_with("v|") && rest.ends_with(dir.file_name().unwrap().to_str().unwrap()), "{rest}");
        // No `$FILE` anywhere: the path is appended.
        std::fs::write(&p, "a").unwrap();
        let f = write_info(vec!["sh", "-c", "printf 'got:%s' \"$0\" > \"$0\"; true"], &[]);
        f.run(&p, &dir);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), format!("got:{}", p.display()));
    }

    #[test]
    fn failures_are_reported_not_raised() {
        let (dir, _) = setup();
        let p = dir.join("a.txt");
        std::fs::write(&p, "keep me").unwrap();
        let mk = |cmd: Vec<&str>| Formatters::from_config(&BTreeMap::from([("bad".to_string(), FormatterConfig { command: cmd.into_iter().map(String::from).collect(), extensions: vec!["txt".into()], ..Default::default() })]), &|_| true);
        let Outcome::Failed { message, .. } = mk(vec!["sh", "-c", "echo 'syntax error on line 3' >&2; echo more >&2; exit 2"]).run(&p, &dir) else { panic!() };
        assert_eq!(message, "exit code 2: syntax error on line 3 | more");
        let Outcome::Failed { message, .. } = mk(vec!["/no/such/formatter"]).run(&p, &dir) else { panic!() };
        assert!(message.starts_with("cannot start '/no/such/formatter'"), "{message}");
        let t = Instant::now();
        let slow = mk(vec!["sh", "-c", "sleep 5"]).with_timeout(Duration::from_millis(300));
        let Outcome::Failed { message, .. } = slow.run(&p, &dir) else { panic!() };
        assert_eq!(message, "timed out after 0 s");
        assert!(t.elapsed() < Duration::from_secs(3));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "keep me", "a failing formatter leaves the file alone");
        assert!(note(&Outcome::Failed { name: "bad".into(), message: "x".into() }).unwrap().contains("left as written"));
        assert!(matches!(Formatters::default().run(&p, &dir), Outcome::NotApplicable));
    }

    #[test]
    fn a_dropped_byte_order_mark_is_put_back() {
        let (dir, _) = setup();
        let p = dir.join("a.txt");
        let strip_bom = FormatterConfig { command: vec!["sh".into(), "-c".into(), "tail -c +4 \"$0\" > \"$0.new\" && mv \"$0.new\" \"$0\"".into()], extensions: vec!["txt".into()], ..Default::default() };
        let f = Formatters::from_config(&BTreeMap::from([("nobom".to_string(), strip_bom)]), &|_| true);
        std::fs::write(&p, b"\xef\xbb\xbfbody").unwrap();
        assert_eq!(f.run(&p, &dir), Outcome::Unchanged, "the formatter removed the BOM, moco restored it: net no change");
        assert_eq!(std::fs::read(&p).unwrap(), b"\xef\xbb\xbfbody");
    }
}
