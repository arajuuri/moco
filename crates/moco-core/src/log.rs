//! Diagnostic log (docs/10-roadmap.md, issue #27): a small file logger with no dependencies.
//!
//! * `MOCO_LOG` (or the `log` config key) is an env_logger-style filter: a level for everything
//!   (`error|warn|info|debug|trace|off`, default `warn`) and optional per-target levels, e.g. `warn,mcp=debug`.
//!   The `classifier` target defaults to `info` so that every automatic permission verdict is on record (issue #28)
//!   without turning the whole log up; name it in the filter to change that.
//! * Lines go to `<app_dir>/data/logs/moco-YYYY-MM-DD.log` (UTC days, mode 0600). Files older than 7 days are
//!   deleted when a file is opened, and one file stops growing at 16 MiB. **Nothing is ever written to the
//!   terminal**: if the file cannot be opened, logging silently stays off.
//! * Every line is passed through `redact`: registered API keys, `Authorization`/`x-api-key` style headers and
//!   bearer tokens never reach the file, not even at `trace`.
//!
//! Until `init` runs nothing is written and `enabled` is a single atomic load, so library users and tests pay nothing.
use std::fmt::Display;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Off = 0,
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Level::Off,
            "error" => Level::Error,
            "warn" | "warning" => Level::Warn,
            "info" => Level::Info,
            "debug" => Level::Debug,
            "trace" => Level::Trace,
            _ => return None,
        })
    }
    fn label(self) -> &'static str {
        match self {
            Level::Off => "OFF  ",
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }
}

/// How much to log: a default level and per-target overrides.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub default: Level,
    pub targets: Vec<(String, Level)>,
}

/// Targets whose default is more verbose than the global default (see the module comment).
const AUDIT_TARGETS: &[&str] = &["classifier"];

impl Default for Filter {
    fn default() -> Self {
        Filter { default: Level::Warn, targets: AUDIT_TARGETS.iter().map(|t| (t.to_string(), Level::Info)).collect() }
    }
}

impl Filter {
    /// `"debug"`, `"warn,mcp=trace"`, `"info,classifier=off"`. A bare level sets the default; `target=level` overrides
    /// one target. Fails with a message naming the bad part.
    pub fn parse(spec: &str) -> Result<Filter, String> {
        let mut f = Filter::default();
        let mut explicit: Vec<String> = vec![];
        for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match part.split_once('=') {
                Some((target, level)) => {
                    let level = Level::parse(level).ok_or_else(|| format!("unknown log level '{}' in '{part}'", level.trim()))?;
                    let target = target.trim().to_string();
                    f.targets.retain(|(t, _)| *t != target);
                    f.targets.push((target.clone(), level));
                    explicit.push(target);
                }
                None => f.default = Level::parse(part).ok_or_else(|| format!("unknown log level '{part}' (use error, warn, info, debug, trace or off)"))?,
            }
        }
        // An audit target is at least `info`, but a more verbose global level applies to it too: `MOCO_LOG=debug`
        // must show the classifier's debug lines. Only an explicit `target=level` overrides this.
        for (t, l) in f.targets.iter_mut() {
            if AUDIT_TARGETS.contains(&t.as_str()) && !explicit.contains(t) {
                *l = f.default.max(Level::Info);
            }
        }
        Ok(f)
    }

    pub fn allows(&self, level: Level, target: &str) -> bool {
        level != Level::Off && level <= self.limit(target)
    }

    fn limit(&self, target: &str) -> Level {
        self.targets.iter().find(|(t, _)| t == target).map_or(self.default, |(_, l)| *l)
    }

    /// The most verbose level any target asks for: the cheap first check of `enabled`.
    fn ceiling(&self) -> Level {
        self.targets.iter().map(|(_, l)| *l).chain(std::iter::once(self.default)).max().unwrap_or(Level::Off)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Redaction

static SECRETS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Remember a credential so that `redact` removes it from every line, wherever it shows up.
pub fn add_secret(secret: &str) {
    let secret = secret.trim();
    // Very short values would blank out ordinary words.
    if secret.len() < 8 {
        return;
    }
    let mut all = SECRETS.lock().unwrap_or_else(|e| e.into_inner());
    if !all.iter().any(|s| s == secret) {
        all.push(secret.to_string());
    }
}

const SENSITIVE_HEADERS: &[&str] = &["authorization", "proxy-authorization", "x-api-key", "x-goog-api-key", "api-key", "cookie", "set-cookie"];

/// Whether the value of this HTTP header must never be logged.
pub fn sensitive_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    SENSITIVE_HEADERS.contains(&n.as_str()) || n.contains("token") || n.contains("secret") || n.contains("api-key") || n.contains("apikey")
}

struct Patterns {
    header: regex::Regex,
    cookie: regex::Regex,
    bearer: regex::Regex,
    prefixed_key: regex::Regex,
    url_param: regex::Regex,
    // Shell command lines (the permission classifier logs and stores them).
    secret_assignment: regex::Regex,
    user_flag: regex::Regex,
    secret_flag: regex::Regex,
    mysql_password: regex::Regex,
    url_credentials: regex::Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        // `x-api-key: abc`, `"authorization": "Bearer abc"`, `api-key=abc`: the value is one token (after an optional
        // scheme word), so the text that follows it on the line is kept.
        header: regex::Regex::new(r#"(?i)\b(authorization|proxy-authorization|x-api-key|x-goog-api-key|api-key)(["']?\s*[:=]\s*["']?)(?:(?:bearer|basic|token)\s+)?[^\s"',;}\]]*"#).unwrap(),
        // A cookie value may hold `;` and spaces, so it is taken to the end of the line.
        cookie: regex::Regex::new(r#"(?i)\b(cookie|set-cookie)(["']?\s*[:=]\s*["']?)[^\r\n"']*"#).unwrap(),
        bearer: regex::Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{8,}").unwrap(),
        prefixed_key: regex::Regex::new(r"\b(sk|pk|rk|xai|gsk|key)-[A-Za-z0-9_-]{16,}").unwrap(),
        url_param: regex::Regex::new(r"(?i)([?&](?:key|api_key|apikey|access_token|token)=)[^&\s\x22']+").unwrap(),
        // `GITHUB_TOKEN=ghp_x`, `export DB_PASSWORD='a b'`, `AWS_SECRET_ACCESS_KEY=...`: an assignment whose name says it is
        // a secret. The value is a quoted string or runs to the next space, `;`, `&` or `|`.
        secret_assignment: regex::Regex::new(r#"(?i)\b((?:[A-Za-z0-9_]*(?:token|secret|passw(?:or)?d|pwd|credential|authorization|_auth\b|api_?key|private_?key|access_?key)[A-Za-z0-9_]*)|(?:[A-Za-z0-9]+_)*key(?:_[A-Za-z0-9]+)*)=("[^"]*"|'[^']*'|[^\s;&|]+)"#).unwrap(),
        // `curl -u user:pass`, `--user=user:pass`. Only a value with a colon: `git push -u origin` is not a credential.
        user_flag: regex::Regex::new(r#"(?i)(\s(?:-u|--user|--proxy-user)(?:\s+|=))("[^"]*:[^"]*"|'[^']*:[^']*'|[^\s"';&|]*:[^\s"';&|]*)"#).unwrap(),
        // `--password hunter2`, `--token=abc`, `--api-key abc`.
        secret_flag: regex::Regex::new(r#"(?i)(--(?:password|passwd|pass|token|secret|api-key|apikey|access-token|auth-token|client-secret)(?:=|\s+))("[^"]*"|'[^']*'|[^\s;&|]+)"#).unwrap(),
        // `mysql -u root -phunter2`: the password is glued to the flag. Only for the mysql family, where `-p` means that.
        mysql_password: regex::Regex::new(r"(?i)(\bmysql\w*\b[^\n;|&]*?\s)-p\S+").unwrap(),
        // `postgres://user:pass@host/db`, `redis://:pass@host`
        url_credentials: regex::Regex::new(r"(?i)\b([a-z][a-z0-9+.-]*://[^\s:/@]*:)[^\s@/]+@").unwrap(),
    })
}

/// `text` with credentials replaced by `[redacted]`.
pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for s in SECRETS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        if out.contains(s.as_str()) {
            out = out.replace(s.as_str(), "[redacted]");
        }
    }
    let p = patterns();
    let out = p.header.replace_all(&out, "$1$2[redacted]");
    let out = p.cookie.replace_all(&out, "$1$2[redacted]");
    let out = p.bearer.replace_all(&out, "Bearer [redacted]");
    let out = p.prefixed_key.replace_all(&out, "[redacted]");
    let out = p.url_param.replace_all(&out, "${1}[redacted]");
    let out = p.secret_assignment.replace_all(&out, "$1=[redacted]");
    let out = p.user_flag.replace_all(&out, "$1[redacted]");
    let out = p.secret_flag.replace_all(&out, "$1[redacted]");
    let out = p.mysql_password.replace_all(&out, "${1}-p[redacted]");
    p.url_credentials.replace_all(&out, "${1}[redacted]@").into_owned()
}

// ---------------------------------------------------------------------------------------------------------------
// Time

pub fn stamp(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (h, m, s) = (secs.rem_euclid(86_400) / 3600, secs.rem_euclid(3600) / 60, secs.rem_euclid(60));
    format!("{}T{h:02}:{m:02}:{s:02}.{:03}Z", crate::agent::date_from_unix(secs), ms.rem_euclid(1000))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------------------------
// The file

const KEEP_DAYS: i64 = 7;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// One record is cut here: a `trace` request body can hold megabytes of base64.
const MAX_RECORD: usize = 32 * 1024;

pub fn log_dir(app_dir: &Path) -> PathBuf {
    app_dir.join("data/logs")
}

fn file_name(day: &str) -> String {
    format!("moco-{day}.log")
}

/// The file a process writing now appends to.
pub fn current_file(app_dir: &Path) -> PathBuf {
    log_dir(app_dir).join(file_name(&crate::agent::date_from_unix(now_ms() / 1000)))
}

/// The newest log file in the directory, if any.
pub fn latest_file(app_dir: &Path) -> Option<PathBuf> {
    let mut names: Vec<String> = std::fs::read_dir(log_dir(app_dir)).ok()?.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).filter(|n| day_of(n).is_some()).collect();
    names.sort();
    names.pop().map(|n| log_dir(app_dir).join(n))
}

fn day_of(name: &str) -> Option<&str> {
    let day = name.strip_prefix("moco-")?.strip_suffix(".log")?;
    (day.len() == 10 && day.bytes().enumerate().all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() })).then_some(day)
}

/// One line per record: control characters and newlines are escaped so that a record is always one line.
fn one_line(msg: &str) -> String {
    let mut cut = msg.len().min(MAX_RECORD);
    while !msg.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + 16);
    for c in msg[..cut].chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => {}
            '\t' => out.push(' '),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    if cut < msg.len() {
        out.push_str(&format!(" ...[{} more bytes]", msg.len() - cut));
    }
    out
}

/// What the first line of each file says: enough to tell logs from the Mac and from Linux apart.
pub fn header() -> String {
    let kernel = {
        let mut u: libc::utsname = unsafe { std::mem::zeroed() };
        if unsafe { libc::uname(&mut u) } == 0 {
            let s = |f: &[libc::c_char]| unsafe { std::ffi::CStr::from_ptr(f.as_ptr()) }.to_string_lossy().into_owned();
            format!("{} {}", s(&u.sysname), s(&u.release))
        } else {
            "unknown".into()
        }
    };
    let product = if cfg!(target_os = "macos") {
        std::process::Command::new("sw_vers").arg("-productVersion").output().ok().filter(|o| o.status.success()).map(|o| format!("macOS {}", String::from_utf8_lossy(&o.stdout).trim()))
    } else {
        None
    };
    format!("moco {} ({}-{}) os=\"{}\" kernel=\"{kernel}\" pid={}", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH, product.unwrap_or_else(|| std::env::consts::OS.to_string()), std::process::id())
}

/// An open day file with the rules for rotating and capping it.
pub struct Sink {
    dir: PathBuf,
    header: String,
    day: String,
    file: Option<std::fs::File>,
    capped: bool,
    /// Opening failed: stay quiet instead of retrying for every line.
    failed: bool,
}

impl Sink {
    pub fn new(dir: PathBuf, header: String) -> Sink {
        Sink { dir, header, day: String::new(), file: None, capped: false, failed: false }
    }

    fn open(&mut self, day: &str, now: i64) {
        use std::os::unix::fs::OpenOptionsExt;
        self.file = None;
        self.day = day.to_string();
        self.capped = false;
        self.failed = false;
        let opened = std::fs::create_dir_all(&self.dir).and_then(|_| std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(self.dir.join(file_name(day))));
        match opened {
            Ok(f) => {
                self.file = Some(f);
                self.prune(now);
                let line = format!("{} INFO  log: {}\n", stamp(now), one_line(&self.header));
                self.append(&line);
            }
            Err(_) => self.failed = true,
        }
    }

    /// Delete day files that are `KEEP_DAYS` or more days old.
    fn prune(&self, now: i64) {
        let cutoff = crate::agent::date_from_unix(now / 1000 - KEEP_DAYS * 86_400);
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        for e in entries.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            if day_of(&name).is_some_and(|d| d <= cutoff.as_str()) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    fn append(&mut self, line: &str) {
        if let Some(f) = &mut self.file {
            // One write call per line, so lines of two moco processes never interleave mid-line.
            let _ = f.write_all(line.as_bytes());
        }
    }

    /// The file's size right now, other moco processes' lines included. Asked before every write, so the cap is shared
    /// by everything that writes the day file (a stale count would let each process add up to the whole cap).
    fn size_now(&self) -> u64 {
        self.file.as_ref().and_then(|f| f.metadata().ok()).map_or(0, |m| m.len())
    }

    /// Append one record stamped `now` (ms since the epoch).
    pub fn write(&mut self, now: i64, level: Level, target: &str, msg: &str) {
        let day = crate::agent::date_from_unix(now / 1000);
        if self.day != day {
            self.open(&day, now);
        }
        if self.file.is_none() || self.failed {
            return;
        }
        let line = format!("{} {} {target}: {}\n", stamp(now), level.label(), one_line(msg));
        if self.size_now() + line.len() as u64 > MAX_FILE_BYTES {
            if !self.capped {
                self.capped = true;
                let note = format!("{} WARN  log: this file reached {} MiB; dropping lines until the next day\n", stamp(now), MAX_FILE_BYTES / 1024 / 1024);
                self.append(&note);
            }
            return;
        }
        self.append(&line);
    }
}

struct Logger {
    filter: Filter,
    sink: Sink,
    /// `MOCO_LOG` was set, so the config key does not override it.
    from_env: bool,
}

static GLOBAL: Mutex<Option<Logger>> = Mutex::new(None);
/// The most verbose level anything is logged at: `enabled` checks it before taking the lock.
static CEILING: AtomicU8 = AtomicU8::new(0);

fn set_ceiling(f: &Filter) {
    CEILING.store(f.ceiling() as u8, Ordering::Relaxed);
}

/// Start logging into `<app_dir>/data/logs`. The filter comes from `MOCO_LOG`; a bad value falls back to the default
/// and says so in the log. Call it once, early; calling it again replaces the logger.
pub fn init(app_dir: &Path) {
    let (spec, from_env) = match std::env::var("MOCO_LOG") {
        Ok(s) if !s.trim().is_empty() => (Some(s), true),
        _ => (None, false),
    };
    let (filter, problem) = match spec.as_deref().map(Filter::parse) {
        Some(Ok(f)) => (f, None),
        Some(Err(e)) => (Filter::default(), Some(format!("MOCO_LOG ignored: {e}"))),
        None => (Filter::default(), None),
    };
    set_ceiling(&filter);
    *GLOBAL.lock().unwrap_or_else(|e| e.into_inner()) = Some(Logger { filter, sink: Sink::new(log_dir(app_dir), header()), from_env });
    if let Some(p) = problem {
        emit(Level::Warn, "log", &p);
    }
}

/// Apply the `log` config key. `MOCO_LOG` wins when it is set.
pub fn configure(spec: Option<&str>) {
    let Some(spec) = spec else { return };
    let mut g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(l) = g.as_mut() else { return };
    if l.from_env {
        return;
    }
    match Filter::parse(spec) {
        Ok(f) => {
            set_ceiling(&f);
            l.filter = f;
        }
        Err(e) => {
            let now = now_ms();
            l.sink.write(now, Level::Warn, "log", &format!("config key `log` ignored: {e}"));
        }
    }
}

/// Whether a record at `level` for `target` would be written. The macros call this first, so a disabled record
/// costs no formatting.
pub fn enabled(level: Level, target: &str) -> bool {
    #[cfg(test)]
    if let Some(r) = capture::enabled(level, target) {
        return r;
    }
    if level as u8 > CEILING.load(Ordering::Relaxed) {
        return false;
    }
    GLOBAL.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|l| l.filter.allows(level, target))
}

/// Write one record (already known to be enabled). The message is redacted first.
pub fn emit(level: Level, target: &str, msg: &str) {
    let msg = redact(msg);
    #[cfg(test)]
    if capture::record(level, target, &msg) {
        return;
    }
    let now = now_ms();
    if let Some(l) = GLOBAL.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        if l.filter.allows(level, target) {
            l.sink.write(now, level, target, &msg);
        }
    }
}

#[macro_export]
macro_rules! log_at {
    ($level:expr, $target:expr, $($arg:tt)*) => {
        if $crate::log::enabled($level, $target) {
            $crate::log::emit($level, $target, &format!($($arg)*));
        }
    };
}
#[macro_export]
macro_rules! log_error { ($target:expr, $($arg:tt)*) => { $crate::log_at!($crate::log::Level::Error, $target, $($arg)*) }; }
#[macro_export]
macro_rules! log_warn { ($target:expr, $($arg:tt)*) => { $crate::log_at!($crate::log::Level::Warn, $target, $($arg)*) }; }
#[macro_export]
macro_rules! log_info { ($target:expr, $($arg:tt)*) => { $crate::log_at!($crate::log::Level::Info, $target, $($arg)*) }; }
#[macro_export]
macro_rules! log_debug { ($target:expr, $($arg:tt)*) => { $crate::log_at!($crate::log::Level::Debug, $target, $($arg)*) }; }
#[macro_export]
macro_rules! log_trace { ($target:expr, $($arg:tt)*) => { $crate::log_at!($crate::log::Level::Trace, $target, $($arg)*) }; }

/// For results that used to be thrown away with `let _ =`: the failure is logged and the caller carries on.
pub trait LogErr<T> {
    fn log_err(self, target: &str, what: &str) -> Option<T>;
}

impl<T, E: Display> LogErr<T> for Result<T, E> {
    fn log_err(self, target: &str, what: &str) -> Option<T> {
        match self {
            Ok(v) => Some(v),
            Err(e) => {
                crate::log_warn!(target, "{what} failed: {e}");
                None
            }
        }
    }
}

/// The tail of a child process's stderr for a log line: the last few lines, trimmed.
pub fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    all[all.len().saturating_sub(lines)..].join(" | ")
}

/// The last `n` lines of a log file. Reads backwards from the end in chunks, so a big file costs only what the tail needs.
pub fn read_tail(path: &Path, n: usize) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    const CHUNK: u64 = 64 * 1024;
    let mut f = std::fs::File::open(path)?;
    let (mut start, mut buf, mut newlines) = (f.metadata()?.len(), Vec::<u8>::new(), 0usize);
    // One newline more than asked for, so that the first line kept is whole.
    while start > 0 && newlines <= n {
        let step = start.min(CHUNK);
        start -= step;
        f.seek(SeekFrom::Start(start))?;
        let mut chunk = vec![0u8; step as usize];
        f.read_exact(&mut chunk)?;
        newlines += chunk.iter().filter(|b| **b == b'\n').count();
        chunk.extend_from_slice(&buf);
        buf = chunk;
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // cut off mid-line by the chunk boundary
    }
    Ok(lines[lines.len().saturating_sub(n)..].iter().map(|l| format!("{l}\n")).collect())
}

/// `moco logs --follow`: new complete lines since the last poll, moving on to the next day's file at midnight.
pub struct Follow {
    app_dir: PathBuf,
    path: Option<PathBuf>,
    pos: u64,
}

impl Follow {
    /// The lines already in the newest file are skipped; files that appear later are read from their start.
    pub fn new(app_dir: &Path) -> Follow {
        Follow { app_dir: app_dir.to_path_buf(), path: None, pos: 0 }
    }

    pub fn poll(&mut self) -> String {
        use std::io::{Read, Seek, SeekFrom};
        let Some(newest) = latest_file(&self.app_dir) else { return String::new() };
        if self.path.as_ref() != Some(&newest) {
            let first = self.path.is_none();
            self.pos = if first { std::fs::metadata(&newest).map(|m| m.len()).unwrap_or(0) } else { 0 };
            self.path = Some(newest.clone());
        }
        let Ok(mut f) = std::fs::File::open(&newest) else { return String::new() };
        if f.seek(SeekFrom::Start(self.pos)).is_err() {
            return String::new();
        }
        let mut buf = vec![];
        if f.read_to_end(&mut buf).is_err() {
            return String::new();
        }
        // Only whole lines: a record being written right now is picked up by the next poll.
        let Some(end) = buf.iter().rposition(|b| *b == b'\n') else { return String::new() };
        self.pos += end as u64 + 1;
        String::from_utf8_lossy(&buf[..=end]).into_owned()
    }
}

/// Test support: records of the current thread go to a list instead of the file.
#[cfg(test)]
pub(crate) mod capture {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static SINK: RefCell<Option<(Filter, Vec<String>)>> = const { RefCell::new(None) };
    }

    /// For code that logs from other threads (the engine runs tools on their own): one buffer for the whole test
    /// process. Tests pick out their own lines by a unique id such as the session id.
    static GLOBAL: Mutex<Option<(Filter, Vec<String>)>> = Mutex::new(None);

    pub(crate) fn ensure_global(spec: &str) {
        let mut g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            *g = Some((Filter::parse(spec).unwrap(), vec![]));
        }
    }

    /// Lines logged anywhere in the process that contain `needle`.
    pub(crate) fn global_lines(needle: &str) -> Vec<String> {
        GLOBAL.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|(_, l)| l.iter().filter(|l| l.contains(needle)).cloned().collect()).unwrap_or_default()
    }

    pub(super) fn enabled(level: Level, target: &str) -> Option<bool> {
        SINK.with(|s| s.borrow().as_ref().map(|(f, _)| f.allows(level, target))).or_else(|| GLOBAL.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|(f, _)| f.allows(level, target)))
    }

    pub(super) fn record(level: Level, target: &str, msg: &str) -> bool {
        let line = |level: Level| format!("{} {target}: {msg}", level.label().trim_end());
        let local = SINK.with(|s| match s.borrow_mut().as_mut() {
            Some((f, lines)) => {
                if f.allows(level, target) {
                    lines.push(line(level));
                }
                true
            }
            None => false,
        });
        if local {
            return true;
        }
        match GLOBAL.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            Some((f, lines)) => {
                if f.allows(level, target) {
                    lines.push(line(level));
                }
                true
            }
            None => false,
        }
    }

    /// Run `f` with a capturing logger at `spec`; returns its result and what it logged on this thread.
    pub(crate) fn with<R>(spec: &str, f: impl FnOnce() -> R) -> (R, Vec<String>) {
        SINK.with(|s| *s.borrow_mut() = Some((Filter::parse(spec).unwrap(), vec![])));
        let r = f();
        let lines = SINK.with(|s| s.borrow_mut().take()).map(|(_, l)| l).unwrap_or_default();
        (r, lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(crate::id::new_id(tag));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn shell_commands_lose_their_credentials_but_not_their_meaning() {
        for (before, secret) in [
            ("export GITHUB_TOKEN=ghp_abcdef123456 && gh api user", "ghp_abcdef123456"),
            ("PASSWORD=hunter2 ./deploy", "hunter2"),
            ("BASIC_AUTH=user:pw123 ./go", "pw123"),
            ("KEY=abcdef ./go", "abcdef"),
            ("SSH_KEY=abcdef ./go", "abcdef"),
            ("DB_PASSWORD='a b c' ./run", "a b c"),
            ("AWS_SECRET_ACCESS_KEY=abc/def+ghi aws s3 ls", "abc/def+ghi"),
            ("API_KEY=\"quoted value\" make", "quoted value"),
            ("curl -u admin:s3cret https://x.example/api", "s3cret"),
            ("curl --user=admin:s3cret https://x.example/api", "s3cret"),
            ("mysql -u root -phunter2 -e 'select 1'", "hunter2"),
            ("cli login --password hunter2", "hunter2"),
            ("cli login --token=abc123xyz", "abc123xyz"),
            ("psql postgres://app:pw0rd@db.example/app", "pw0rd"),
            ("redis-cli -u redis://:pw0rd@cache:6379 ping", "pw0rd"),
        ] {
            let after = redact(before);
            assert!(!after.contains(secret), "{secret} survived in: {after}");
            assert!(after.contains("[redacted]"), "{after}");
        }
        // What stays readable: the command, its other arguments and the name of the variable.
        assert_eq!(redact("PASSWORD=hunter2 ./deploy --env prod"), "PASSWORD=[redacted] ./deploy --env prod");
        assert!(redact("curl -u admin:s3cret https://x.example/api").starts_with("curl -u [redacted] https://x.example"));
        assert!(redact("psql postgres://app:pw0rd@db.example/app").contains("postgres://app:[redacted]@db.example/app"));
        // Ordinary commands are untouched.
        for plain in ["git push -u origin main", "cargo test -p moco-core", "ls -la /etc", "KEYBOARD=us make", "MONKEY=1 ./go", "mysql -u root -p -e 'select 1'", "git log --author=me", "GIT_AUTHOR_NAME=me git commit", "docker run -u 1000 img", "echo $HOME", "https://example.com/a:b@c"] {
            assert_eq!(redact(plain), plain, "{plain} was changed");
        }
    }

    #[test]
    fn classifier_lines_logged_at_the_default_level_carry_no_credentials() {
        let ((), lines) = capture::with("warn", || {
            crate::log_info!("classifier", "session=ses_x decision=allow command=PASSWORD=hunter2 ./deploy && curl -u admin:s3cret https://x.example");
        });
        assert_eq!(lines.len(), 1, "the classifier target is on at info even when the default is warn: {lines:?}");
        assert!(!lines[0].contains("hunter2") && !lines[0].contains("s3cret"), "{}", lines[0]);
        assert!(lines[0].contains("PASSWORD=[redacted]") && lines[0].contains("./deploy"), "{}", lines[0]);
    }

    #[test]
    fn filters_parse_levels_and_targets() {
        let f = Filter::parse("debug,mcp=trace").unwrap();
        assert!(f.allows(Level::Debug, "engine") && !f.allows(Level::Trace, "engine"));
        assert!(f.allows(Level::Trace, "mcp"));
        let d = Filter::default();
        assert!(d.allows(Level::Warn, "engine") && !d.allows(Level::Info, "engine"), "warn by default");
        assert!(d.allows(Level::Info, "classifier") && !d.allows(Level::Debug, "classifier"), "verdicts are on record by default");
        assert!(!Filter::parse("info,classifier=off").unwrap().allows(Level::Error, "classifier"));
        let dbg = Filter::parse("debug").unwrap();
        assert!(dbg.allows(Level::Debug, "classifier") && !dbg.allows(Level::Trace, "classifier"), "a verbose global level reaches the audit target");
        let warn = Filter::parse("warn").unwrap();
        assert!(warn.allows(Level::Info, "classifier") && !warn.allows(Level::Debug, "classifier"), "the audit floor stays at info");
        let quiet = Filter::parse("trace,classifier=warn").unwrap();
        assert!(quiet.allows(Level::Warn, "classifier") && !quiet.allows(Level::Info, "classifier"), "an explicit setting wins");
        assert!(!Filter::parse("off").unwrap().allows(Level::Error, "engine"));
        assert!(Filter::parse("loud").unwrap_err().contains("loud"));
        assert!(Filter::parse("warn,mcp=loud").is_err());
        assert_eq!(Filter::parse("").unwrap(), Filter::default());
        assert_eq!(Filter::parse("trace").unwrap().ceiling(), Level::Trace);
    }

    #[test]
    fn redaction_covers_headers_tokens_and_registered_keys() {
        add_secret("k-registered-0123456789");
        let r = redact(r#"POST x-api-key: sk-ant-abcdefghijklmnopqrstuv
Authorization: Bearer abcdef123456789
{"authorization":"Bearer zzzzzzzzzzzz","note":"keep me","key":"k-registered-0123456789"}
https://h/p?key=AIzaSECRET&alt=sse and sk-proj-abcdefghijklmnopqrstu"#);
        for leaked in ["abcdefghijklmnopqrstuv", "abcdef123456789", "zzzzzzzzzzzz", "k-registered-0123456789", "AIzaSECRET", "abcdefghijklmnopqrstu"] {
            assert!(!r.contains(leaked), "{leaked} leaked: {r}");
        }
        assert!(r.contains("keep me") && r.contains("alt=sse"), "ordinary text survives: {r}");
        let r2 = redact("headers=[x-api-key: abcdefghijkl; accept: json] body={\"a\":1} cookie: s=1; t=2");
        assert!(r2.contains("accept: json] body={\"a\":1}") && !r2.contains("abcdefghijkl") && !r2.contains("t=2"), "{r2}");
        assert!(sensitive_header("X-Api-Key") && sensitive_header("Authorization") && sensitive_header("x-auth-token") && !sensitive_header("content-type"));
        add_secret("short");
        assert_eq!(redact("a short word"), "a short word", "values under 8 characters are not registered");
    }

    #[test]
    fn records_are_one_line_cut_and_escaped() {
        assert_eq!(one_line("a\nb\r\tc\u{7}d"), "a\\nb c d");
        let long = "x".repeat(MAX_RECORD + 100);
        assert!(one_line(&long).ends_with("...[100 more bytes]"));
        assert_eq!(tail("a\n\nb\nc\nd\n", 2), "c | d");
        // A cut inside a multi-byte character must not panic.
        let wide = "é".repeat(MAX_RECORD);
        assert!(one_line(&wide).contains("more bytes"));
    }

    #[test]
    fn a_file_per_day_with_a_header_and_old_days_removed() {
        let dir = tmp("log-days");
        let day = |d: i64| d * 86_400_000 + 3_600_000; // 01:00 UTC of day number d
        let d0 = 20_000; // 2024-10-04
        for old in 1..=9 {
            std::fs::write(dir.join(file_name(&crate::agent::date_from_unix((d0 - old) * 86_400))), "old\n").unwrap();
        }
        std::fs::write(dir.join("notes.txt"), "mine").unwrap();
        let mut sink = Sink::new(dir.clone(), "moco test header".into());
        sink.write(day(d0), Level::Warn, "engine", "first\nsecond");
        sink.write(day(d0) + 5, Level::Error, "store", "boom");
        sink.write(day(d0 + 1), Level::Warn, "engine", "next day");
        let today = std::fs::read_to_string(dir.join(file_name(&crate::agent::date_from_unix(d0 * 86_400)))).unwrap();
        let lines: Vec<&str> = today.lines().collect();
        assert_eq!(lines.len(), 3, "{today}");
        assert!(lines[0].starts_with("2024-10-04T01:00:00.000Z INFO  log: moco test header"), "{}", lines[0]);
        assert!(lines[1].ends_with("WARN  engine: first\\nsecond"), "{}", lines[1]);
        assert!(lines[2].contains("ERROR store: boom") && lines[2].starts_with("2024-10-04T01:00:00.005Z"), "{}", lines[2]);
        let next = std::fs::read_to_string(dir.join(file_name(&crate::agent::date_from_unix((d0 + 1) * 86_400)))).unwrap();
        assert!(next.starts_with("2024-10-05T01:00:00.000Z INFO  log: moco test header"), "each file starts with the header");
        let mut kept: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        kept.sort();
        // Opening the file of day d0+1 keeps seven days: itself and the six before it (d0-5 ..= d0+1).
        assert!(kept.contains(&"notes.txt".to_string()), "only log files are pruned");
        assert_eq!(kept.iter().filter(|n| day_of(n).is_some()).count(), 7, "{kept:?}");
        assert!(!kept.contains(&file_name(&crate::agent::date_from_unix((d0 - 6) * 86_400))) && kept.contains(&file_name(&crate::agent::date_from_unix((d0 - 5) * 86_400))));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_stops_at_the_size_cap_and_says_so_once() {
        let dir = tmp("log-cap");
        let mut sink = Sink::new(dir.clone(), "h".into());
        let big = "y".repeat(MAX_RECORD - 100);
        for _ in 0..(MAX_FILE_BYTES as usize / MAX_RECORD + 20) {
            sink.write(1_000_000, Level::Warn, "t", &big);
        }
        let path = dir.join(file_name("1970-01-01"));
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size <= MAX_FILE_BYTES + 200, "{size}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("reached 16 MiB").count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn processes_share_the_size_cap_of_a_day_file() {
        let dir = tmp("log-shared-cap");
        let big = "z".repeat(MAX_RECORD - 100);
        let (mut a, mut b) = (Sink::new(dir.clone(), "a".into()), Sink::new(dir.clone(), "b".into()));
        // Process A fills most of the file. Process B opens it afterwards and has never seen those lines.
        for _ in 0..(MAX_FILE_BYTES as usize / MAX_RECORD * 3 / 4) {
            a.write(1_000_000, Level::Warn, "t", &big);
        }
        for _ in 0..(MAX_FILE_BYTES as usize / MAX_RECORD) {
            b.write(1_000_000, Level::Warn, "t", &big);
        }
        for _ in 0..20 {
            a.write(1_000_000, Level::Warn, "t", &big);
        }
        let size = std::fs::metadata(dir.join(file_name("1970-01-01"))).unwrap().len();
        assert!(size <= MAX_FILE_BYTES + 400, "two writers together stay within the cap: {size}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_tail_of_a_big_file_is_read_from_the_end_and_cut_on_line_boundaries() {
        let dir = tmp("log-tail");
        let path = dir.join("big.log");
        // 60k lines of varying length: far more than one 64 KiB chunk, so chunk edges fall inside lines.
        let body: String = (0..60_000).map(|i| format!("line {i} {}\n", "x".repeat(i % 37))).collect();
        std::fs::write(&path, &body).unwrap();
        let expect = |n: usize| -> String { body.lines().rev().take(n).collect::<Vec<_>>().into_iter().rev().map(|l| format!("{l}\n")).collect() };
        for n in [1, 2, 3, 1000, 5000, 59_999, 60_000, 70_000] {
            assert_eq!(read_tail(&path, n).unwrap(), expect(n), "n={n}");
        }
        std::fs::write(&path, "no newline at the end").unwrap();
        assert_eq!(read_tail(&path, 3).unwrap(), "no newline at the end\n");
        std::fs::write(&path, "").unwrap();
        assert_eq!(read_tail(&path, 3).unwrap(), "");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tail_and_follow_read_whole_lines_and_cross_midnight() {
        let app = tmp("log-follow");
        let dir = log_dir(&app);
        let mut sink = Sink::new(dir.clone(), "h".into());
        for i in 0..5 {
            sink.write(86_400_000 * 20_000 + i, Level::Warn, "t", &format!("line {i}"));
        }
        let path = latest_file(&app).unwrap();
        assert!(path.ends_with("moco-2024-10-04.log"));
        let tail = read_tail(&path, 2).unwrap();
        assert!(tail.contains("line 3") && tail.contains("line 4") && !tail.contains("line 2"), "{tail}");
        let mut from_end = Follow::new(&app);
        assert_eq!(from_end.poll(), "", "existing lines are skipped");
        sink.write(86_400_000 * 20_000 + 9, Level::Warn, "t", "fresh");
        assert!(from_end.poll().contains("fresh"));
        // A half-written record is held back until its newline arrives.
        let mut raw = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        raw.write_all(b"2024 half").unwrap();
        assert_eq!(from_end.poll(), "");
        raw.write_all(b" done\n").unwrap();
        assert_eq!(from_end.poll(), "2024 half done\n");
        sink.write(86_400_000 * 20_001, Level::Warn, "t", "next day");
        let moved = from_end.poll();
        assert!(moved.contains("INFO  log: h") && moved.contains("next day"), "the new file is read from its start: {moved}");
        let _ = std::fs::remove_dir_all(app);
    }

    #[test]
    fn an_unwritable_directory_stays_silent() {
        let dir = tmp("log-bad");
        std::fs::write(dir.join("logs"), "a file where the directory should be").unwrap();
        let mut sink = Sink::new(dir.join("logs"), "h".into());
        sink.write(1_000, Level::Error, "t", "nowhere to go"); // must not panic or print
        assert!(sink.failed);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("log-mode");
        let mut sink = Sink::new(dir.clone(), "h".into());
        sink.write(1_000, Level::Warn, "t", "x");
        assert_eq!(std::fs::metadata(dir.join(file_name("1970-01-01"))).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn log_err_logs_and_carries_on_and_macros_skip_disabled_records() {
        let (v, lines) = capture::with("warn", || {
            let a: Result<u8, String> = Err("disk full".into());
            let b: Result<u8, String> = Ok(3);
            let formatted = std::cell::Cell::new(0);
            let expensive = || {
                formatted.set(formatted.get() + 1);
                "x"
            };
            crate::log_debug!("store", "{}", expensive());
            (a.log_err("store", "saving"), b.log_err("store", "ok"), formatted.get())
        });
        assert_eq!(v, (None, Some(3), 0), "a disabled record is not even formatted");
        assert_eq!(lines, ["WARN store: saving failed: disk full"]);
    }
}
