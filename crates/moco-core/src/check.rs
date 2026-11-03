//! Check hooks (issue #26): after a step in which the model changed files, each configured check (a compiler, linter or
//! type-checker) that covers a changed file runs once, and what it reports goes back to the model in the same tool
//! result. A check that times out or cannot start becomes a one-line note, never a tool failure.
use crate::config::CheckConfig;
use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: String,
    pub command: Vec<String>,
    /// Lowercase, with the leading dot.
    pub extensions: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub timeout: Duration,
}

/// What one check had to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Passed,
    /// The check failed; the diagnostics, changed files first, within the caps.
    Errors(String),
    /// It could not be run or timed out (one line).
    Note(String),
    /// The run was cancelled; nothing to report.
    Cancelled,
}

/// One check that ran, with the changed files it covered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub name: String,
    pub files: Vec<PathBuf>,
    pub verdict: Verdict,
}

#[derive(Debug, Clone)]
pub struct Checks {
    list: Vec<Check>,
    /// Most files handed to a `$FILES` command.
    max_files: usize,
    max_lines: usize,
    max_bytes: usize,
}

impl Default for Checks {
    fn default() -> Self {
        Checks { list: vec![], max_files: 20, max_lines: 50, max_bytes: 8 * 1024 }
    }
}

fn norm_ext(e: &str) -> String {
    let e = e.trim().to_lowercase();
    if e.starts_with('.') { e } else { format!(".{e}") }
}

impl Checks {
    /// Build from config. `allowed` decides per check name whether it may run (project-defined ones need approval).
    pub fn from_config(map: &BTreeMap<String, CheckConfig>, allowed: &dyn Fn(&str) -> bool) -> Checks {
        let list = map
            .iter()
            .filter(|(name, c)| !c.disabled && !c.command.is_empty() && !c.extensions.is_empty() && allowed(name))
            .map(|(name, c)| Check {
                name: name.clone(),
                command: c.command.clone(),
                extensions: c.extensions.iter().map(|e| norm_ext(e)).collect(),
                environment: c.environment.clone(),
                timeout: Duration::from_millis(c.timeout.unwrap_or(60_000)),
            })
            .collect();
        Checks { list, ..Default::default() }
    }

    /// Cap the files passed to a check and the diagnostics kept from it.
    pub fn with_limits(mut self, max_files: usize, max_lines: usize, max_bytes: usize) -> Self {
        (self.max_files, self.max_lines, self.max_bytes) = (max_files.max(1), max_lines.max(1), max_bytes.max(1));
        self
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn names(&self) -> Vec<&str> {
        self.list.iter().map(|c| c.name.as_str()).collect()
    }

    /// Run every check that covers at least one of `changed` (absolute paths), each exactly once, in name order.
    pub fn run(&self, changed: &[PathBuf], cwd: &Path, cancel: &AtomicBool) -> Vec<Run> {
        let mut runs = vec![];
        for c in &self.list {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let files: Vec<PathBuf> = changed.iter().filter(|f| c.covers(f)).cloned().collect();
            if files.is_empty() {
                continue;
            }
            let verdict = self.run_one(c, &files, cwd, cancel);
            runs.push(Run { name: c.name.clone(), files, verdict });
        }
        runs
    }

    fn run_one(&self, c: &Check, files: &[PathBuf], cwd: &Path, cancel: &AtomicBool) -> Verdict {
        let mut args: Vec<String> = vec![];
        for a in &c.command {
            if a == "$FILES" {
                args.extend(files.iter().take(self.max_files).map(|f| f.to_string_lossy().into_owned()));
            } else {
                args.push(a.clone());
            }
        }
        let log = std::env::temp_dir().join(format!("{}.log", crate::id::new_id("chk")));
        let Ok(out) = std::fs::File::create(&log) else { return Verdict::Note("no temporary file for the output".into()) };
        let Ok(err) = out.try_clone() else { return Verdict::Note("no temporary file for the output".into()) };
        let child = Command::new(&args[0]).args(&args[1..]).current_dir(cwd).envs(&c.environment).stdin(Stdio::null()).stdout(Stdio::from(out)).stderr(Stdio::from(err)).process_group(0).spawn();
        let mut child = match child {
            Ok(ch) => ch,
            Err(e) => {
                let _ = std::fs::remove_file(&log);
                return Verdict::Note(format!("cannot start '{}': {e}", args[0]));
            }
        };
        let pid = child.id();
        let start = Instant::now();
        enum End {
            Exit(std::process::ExitStatus),
            Timeout,
            Cancelled,
            Lost,
        }
        let end = loop {
            match child.try_wait() {
                Ok(Some(s)) => break End::Exit(s),
                Ok(None) if cancel.load(Ordering::Relaxed) => break End::Cancelled,
                Ok(None) if start.elapsed() >= c.timeout => break End::Timeout,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break End::Lost,
            }
        };
        if !matches!(end, End::Exit(_)) {
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            let _ = child.wait();
        }
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) }; // anything it left behind
        let output = read_capped(&log);
        let _ = std::fs::remove_file(&log);
        match end {
            End::Cancelled => Verdict::Cancelled,
            End::Timeout => Verdict::Note(format!("timed out after {} ms; no diagnostics", c.timeout.as_millis())),
            End::Lost => Verdict::Note("the process could not be waited for".into()),
            End::Exit(s) if s.success() => Verdict::Passed,
            End::Exit(s) => {
                let text = select(&output, files, cwd, self.max_lines, self.max_bytes);
                if text.trim().is_empty() {
                    Verdict::Note(format!("exit code {} with no output", s.code().map_or("?".to_string(), |c| c.to_string())))
                } else {
                    Verdict::Errors(text)
                }
            }
        }
    }
}

impl Check {
    fn covers(&self, path: &Path) -> bool {
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_lowercase()) else { return false };
        self.extensions.iter().any(|e| name.ends_with(e.as_str()) && name.len() > e.len())
    }
}

/// At most 1 MiB of a check's output; a runaway compiler must not fill memory.
fn read_capped(path: &Path) -> String {
    use std::io::Read;
    let mut buf = vec![];
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.take(1 << 20).read_to_end(&mut buf);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Keep the output's lines that mention a changed file first (indented lines belong to the line above), then the
/// rest, up to `max_lines` lines and `max_bytes` bytes; say how much was left out.
pub fn select(output: &str, changed: &[PathBuf], cwd: &Path, max_lines: usize, max_bytes: usize) -> String {
    let needles: Vec<String> = changed.iter().flat_map(|f| [f.to_string_lossy().into_owned(), f.strip_prefix(cwd).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default()]).filter(|n| !n.is_empty()).collect();
    let (mut mine, mut others) = (vec![], vec![]);
    let mut current_mine = false;
    for line in output.lines().filter(|l| !l.trim().is_empty()) {
        if !line.starts_with([' ', '\t']) {
            current_mine = needles.iter().any(|n| line.contains(n.as_str()));
        }
        if current_mine { mine.push(line) } else { others.push(line) }
    }
    let total = mine.len() + others.len();
    let (mut kept, mut bytes) = (vec![], 0usize);
    for line in mine.into_iter().chain(others) {
        if kept.len() >= max_lines || bytes + line.len() + 1 > max_bytes {
            break;
        }
        bytes += line.len() + 1;
        kept.push(line);
    }
    let mut text = kept.join("\n");
    if kept.len() < total {
        text.push_str(&format!("\n... {} more line(s) omitted", total - kept.len()));
    }
    text
}

/// The text added to a tool's result for one run. `after_failure` is true when this check failed the last time it ran,
/// so that a pass is worth mentioning.
pub fn note(run: &Run, after_failure: bool) -> Option<String> {
    match &run.verdict {
        Verdict::Errors(t) => Some(format!("Check {} reported problems (changed files first):\n{t}", run.name)),
        Verdict::Passed if after_failure => Some(format!("Check {} passed.", run.name)),
        Verdict::Note(n) => Some(format!("Check {} did not complete: {n}.", run.name)),
        Verdict::Passed | Verdict::Cancelled => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn config(command: &str, ext: &str, timeout: Option<u64>) -> BTreeMap<String, CheckConfig> {
        BTreeMap::from([("fake".to_string(), CheckConfig { command: vec!["sh".into(), "-c".into(), command.into(), "sh".into(), "$FILES".into()], extensions: vec![ext.into()], timeout, ..Default::default() })])
    }

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(crate::id::new_id("chk-test"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A checker that records one line per run (its arguments) in `runs.log` and prints a problem for every file.
    pub(crate) fn recording_script(dir: &Path) -> String {
        format!("echo \"$*\" >> {}/runs.log; for f in \"$@\"; do echo \"$f:1:1: error: broken\"; done; echo 'other.rs:9:9: error: elsewhere'; exit 1", dir.display())
    }

    #[test]
    fn runs_once_with_only_the_changed_files_it_covers() {
        let d = dir();
        let checks = Checks::from_config(&config(&recording_script(&d), "rs", None), &|_| true);
        let changed = vec![d.join("a.rs"), d.join("b.rs"), d.join("notes.txt")];
        let runs = checks.run(&changed, &d, &AtomicBool::new(false));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].files, [d.join("a.rs"), d.join("b.rs")]);
        let log = std::fs::read_to_string(d.join("runs.log")).unwrap();
        assert_eq!(log.lines().count(), 1, "one process for two files");
        assert!(log.contains("a.rs") && log.contains("b.rs") && !log.contains("notes.txt"), "{log}");
        let Verdict::Errors(t) = &runs[0].verdict else { panic!("{:?}", runs[0].verdict) };
        let first = t.lines().next().unwrap();
        assert!(first.contains("a.rs"), "changed files come first: {t}");
        assert!(t.contains("other.rs:9:9"), "the rest follows");
        assert!(t.lines().position(|l| l.contains("other.rs")).unwrap() > t.lines().position(|l| l.contains("b.rs")).unwrap());
        // Nothing covered: not run at all.
        assert!(checks.run(&[d.join("x.txt")], &d, &AtomicBool::new(false)).is_empty());
        assert_eq!(std::fs::read_to_string(d.join("runs.log")).unwrap().lines().count(), 1);
    }

    #[test]
    fn the_file_count_and_the_output_are_capped() {
        let d = dir();
        let checks = Checks::from_config(&config(&recording_script(&d), "rs", None), &|_| true).with_limits(2, 2, 8 * 1024);
        let changed: Vec<PathBuf> = (0..5).map(|i| d.join(format!("f{i}.rs"))).collect();
        let runs = checks.run(&changed, &d, &AtomicBool::new(false));
        let log = std::fs::read_to_string(d.join("runs.log")).unwrap();
        assert_eq!(log.split_whitespace().count(), 2, "only two files are passed: {log}");
        let Verdict::Errors(t) = &runs[0].verdict else { panic!() };
        assert_eq!(t.lines().count(), 3, "two lines and the omission note: {t}");
        assert!(t.contains("more line(s) omitted"));
        // The byte cap.
        let long = "x".repeat(100);
        let out = (0..50).map(|i| format!("{long}{i}")).collect::<Vec<_>>().join("\n");
        let t = select(&out, &[], &d, 1000, 1000);
        assert!(t.len() < 1100 && t.contains("omitted"), "{}", t.len());
    }

    #[test]
    fn a_timeout_becomes_a_note_and_does_not_hang() {
        let d = dir();
        let checks = Checks::from_config(&config("sleep 5", "rs", Some(300)), &|_| true);
        let t = Instant::now();
        let runs = checks.run(&[d.join("a.rs")], &d, &AtomicBool::new(false));
        assert!(t.elapsed() < Duration::from_secs(3));
        let Verdict::Note(n) = &runs[0].verdict else { panic!("{:?}", runs[0]) };
        assert!(n.contains("timed out after 300 ms"), "{n}");
        assert!(note(&runs[0], false).unwrap().contains("did not complete"));
    }

    #[test]
    fn start_failure_and_silent_failure_are_notes_and_success_is_quiet() {
        let d = dir();
        let mk = |cmd: Vec<&str>| Checks::from_config(&BTreeMap::from([("c".to_string(), CheckConfig { command: cmd.into_iter().map(String::from).collect(), extensions: vec![".rs".into()], ..Default::default() })]), &|_| true);
        let files = [d.join("a.rs")];
        let Verdict::Note(n) = &mk(vec!["/no/such/checker"]).run(&files, &d, &AtomicBool::new(false))[0].verdict.clone() else { panic!() };
        assert!(n.starts_with("cannot start '/no/such/checker'"), "{n}");
        let Verdict::Note(n) = &mk(vec!["sh", "-c", "exit 3"]).run(&files, &d, &AtomicBool::new(false))[0].verdict.clone() else { panic!() };
        assert_eq!(n, "exit code 3 with no output");
        let ok = mk(vec!["sh", "-c", "true"]).run(&files, &d, &AtomicBool::new(false));
        assert_eq!(ok[0].verdict, Verdict::Passed);
        assert_eq!(note(&ok[0], false), None, "a pass is silent...");
        assert_eq!(note(&ok[0], true).unwrap(), "Check c passed.", "...unless it follows a failure");
    }

    #[test]
    fn cancel_stops_a_running_check() {
        let d = dir();
        let checks = Checks::from_config(&config("sleep 5", "rs", None), &|_| true);
        let cancel = AtomicBool::new(true);
        assert!(checks.run(&[d.join("a.rs")], &d, &cancel).is_empty(), "cancelled before it starts");
    }

    #[test]
    fn a_check_that_is_not_allowed_is_never_built_or_run() {
        let d = dir();
        let mut map = config(&recording_script(&d), "rs", None);
        map.insert("other".into(), map["fake"].clone());
        let checks = Checks::from_config(&map, &|name| name == "other");
        assert_eq!(checks.names(), ["other"]);
        let none = Checks::from_config(&map, &|_| false);
        assert!(none.is_empty());
        assert!(none.run(&[d.join("a.rs")], &d, &AtomicBool::new(false)).is_empty());
        assert!(!d.join("runs.log").exists(), "an unapproved check never ran");
        let mut off = map.clone();
        off.get_mut("fake").unwrap().disabled = true;
        assert_eq!(Checks::from_config(&off, &|n| n == "fake").names().len(), 0);
    }
}
