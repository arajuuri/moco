//! moco: terminal chat (full screen on a terminal; line based with --plain, with -p or when piped).
mod tui;
use moco_core::catalog::{Catalog, ProviderInfo};
use moco_core::mcp::{self, oauth::LoginIo, trust, Manager, ServerSpec};
use moco_core::tools::SubagentRequest;
use moco_core::{commands, compaction, mentions, skills};
use moco_core::engine::{CompactResult, DiffResult, Engine, UndoReport, UndoResult, EngineConfig, PermissionPrompt, Reply, RunOutcome, Ui, UiEvent};
use moco_core::provider::registry;
use moco_core::provider::Usage;
use moco_core::tools::{Question, Todo};
use moco_core::store::{Mode, Session, SessionSummary, Store};
use moco_core::{auth, config, permission};
use std::io::{BufRead, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

static CANCEL: AtomicBool = AtomicBool::new(false);
/// True while the chat is waiting for you to type (so a background notice can redraw the prompt).
static AT_PROMPT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_: libc::c_int) {
    CANCEL.store(true, Ordering::Relaxed);
}

/// SIGINT sets the cancel flag; no SA_RESTART so a blocked read returns instead of hanging.
fn install_sigint() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_sigint as extern "C" fn(libc::c_int) as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }
}

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const RED: &str = "\x1b[31m";

#[derive(Default)]
struct RunUsage {
    usage: Usage,
    cost: f64,
    priced: bool,
    session_cost: f64,
    /// Spent this turn on classifying shell commands (also part of the session total, not of `cost`).
    classifier: f64,
    context: u64,
    limit: Option<u64>,
    steps: u32,
}

struct Terminal {
    mid_line: AtomicBool,
    run: Mutex<RunUsage>,
}

impl Terminal {
    fn read_line(&self, prompt: &str) -> Option<String> {
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_string()),
            Err(_) => Some(String::new()), // interrupted by Ctrl-C
        }
    }
    fn newline_if_needed(&self) {
        if self.mid_line.swap(false, Ordering::Relaxed) {
            println!();
        }
    }
    /// One dim line after a turn: tokens, cost, session cost, context fill.
    fn print_usage(&self) {
        let r = std::mem::take(&mut *self.run.lock().unwrap());
        if r.steps == 0 {
            return;
        }
        let mut cost = if r.priced { format!("${:.4} (session ${:.4})", r.cost, r.session_cost) } else { "cost n/a".to_string() };
        if r.classifier > 0.0 {
            cost.push_str(&format!(" + classifier ${:.4}", r.classifier));
        }
        let ctx = match r.limit {
            Some(l) if l > 0 => format!("context {}% of {}", (r.context * 100 / l).min(100), human(l)),
            _ => format!("context {}", human(r.context)),
        };
        let wrote = if r.usage.cache_write > 0 { format!(", {} cache written", human(r.usage.cache_write)) } else { String::new() };
        println!("{DIM}{} in, {} out, {} cached{wrote} | {cost} | {ctx}{RESET}", human(r.usage.input), human(r.usage.output), human(r.usage.cache_read));
    }
}

fn human(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{:.1}k", n as f64 / 1000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// One-line description of a tool call for the transcript.
fn summary(name: &str, input: &serde_json::Value) -> String {
    let get = |k: &str| input[k].as_str().unwrap_or("").to_string();
    let text = match name {
        "shell" => get("command"),
        "glob" | "grep" => get("pattern"),
        "webfetch" => get("url"),
        "websearch" => get("query"),
        "subagent" => format!("{}: {}", get("agent"), get("description")),
        "question" => input["questions"][0]["question"].as_str().unwrap_or("").to_string(),
        "todowrite" => format!("{} item(s)", input["todos"].as_array().map_or(0, |t| t.len())),
        "patch" => {
            let patch = get("patchText");
            let files: Vec<&str> = patch
                .lines()
                .filter_map(|l| l.strip_prefix("*** ").and_then(|r| r.split_once(" File: ")).map(|(_, path)| path))
                .collect();
            files.join(", ")
        }
        _ => get("path"),
    };
    let first = text.lines().next().unwrap_or("");
    if first.chars().count() > 100 { format!("{}...", first.chars().take(100).collect::<String>()) } else { first.to_string() }
}

impl Terminal {
    /// Events from subagent sessions: tool activity indented, cost counted, assistant text hidden.
    fn nested(&self, depth: usize, agent: &str, e: &UiEvent) {
        let pad = "  ".repeat(depth);
        match e {
            UiEvent::Nested { agent, inner } => self.nested(depth + 1, agent, inner),
            UiEvent::ToolStarted { name, input, .. } => {
                self.newline_if_needed();
                println!("{DIM}{pad}[{agent}] > {name} {}{RESET}", summary(name, input));
            }
            UiEvent::ToolFinished { ok: false, text, .. } => println!("{RED}{pad}  failed: {}{RESET}", text.lines().next().unwrap_or("")),
            UiEvent::StepUsage { usage, cost, .. } => {
                let mut r = self.run.lock().unwrap();
                r.usage.input += usage.input;
                r.usage.output += usage.output;
                r.usage.cache_read += usage.cache_read;
                r.usage.cache_write += usage.cache_write;
                r.cost += cost.unwrap_or(0.0);
                r.priced |= cost.is_some();
                r.steps += 1;
            }
            UiEvent::ClassifierUsage { cost, .. } => self.run.lock().unwrap().classifier += cost,
            UiEvent::Retry { attempt, wait, error } => println!("{DIM}{pad}[{agent}] retrying in {}s (attempt {attempt}): {error}{RESET}", wait.as_secs()),
            UiEvent::Notice(n) => eprintln!("{RED}{pad}[{agent}] {n}{RESET}"),
            _ => {}
        }
        let _ = std::io::stdout().flush();
    }
}

fn print_todos(todos: &[Todo]) {
    for t in todos {
        let mark = match t.status.as_str() {
            "completed" => "[x]",
            "in_progress" => "[~]",
            "cancelled" => "[-]",
            _ => "[ ]",
        };
        println!("{DIM}  {mark} {}{RESET}", t.content);
    }
}

impl Ui for Terminal {
    fn ask_questions(&self, questions: &[Question]) -> Option<Vec<Vec<String>>> {
        self.newline_if_needed();
        let mut answers = vec![];
        for q in questions {
            println!("{}{}", if q.header.is_empty() { String::new() } else { format!("[{}] ", q.header) }, q.question);
            for (i, o) in q.options.iter().enumerate() {
                println!("  {}. {}{}", i + 1, o.label, if o.description.is_empty() { String::new() } else { format!(" - {}", o.description) });
            }
            let hint = if q.multiple { "numbers separated by commas" } else { "a number" };
            let line = self.read_line(&format!("  answer ({hint}, or type your own; Enter to dismiss): "))?;
            if line.trim().is_empty() {
                return None;
            }
            let picked: Vec<String> = line
                .split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(|t| match t.parse::<usize>() {
                    Ok(n) if n >= 1 && n <= q.options.len() => q.options[n - 1].label.clone(),
                    _ => t.to_string(),
                })
                .take(if q.multiple { usize::MAX } else { 1 })
                .collect();
            answers.push(picked);
        }
        Some(answers)
    }

    fn event(&self, e: &UiEvent) {
        match e {
            UiEvent::Nested { agent, inner } => {
                self.nested(1, agent, inner);
                return;
            }
            UiEvent::Todos(t) => {
                self.newline_if_needed();
                print_todos(t);
            }
            UiEvent::TextDelta(t) => {
                print!("{t}");
                self.mid_line.store(!t.ends_with('\n'), Ordering::Relaxed);
            }
            UiEvent::ReasoningDelta(_) | UiEvent::Titled(_) | UiEvent::Delivered => return,
            UiEvent::StepUsage { usage, cost, session_cost, context_tokens, context_limit } => {
                let mut r = self.run.lock().unwrap();
                r.usage.input += usage.input;
                r.usage.output += usage.output;
                r.usage.cache_read += usage.cache_read;
                r.usage.cache_write += usage.cache_write;
                r.priced |= cost.is_some();
                r.cost += cost.unwrap_or(0.0);
                r.session_cost = *session_cost;
                r.context = *context_tokens;
                r.limit = *context_limit;
                r.steps += 1;
                return;
            }
            UiEvent::ClassifierUsage { cost, session_cost, .. } => {
                let mut r = self.run.lock().unwrap();
                r.classifier += cost;
                r.session_cost = *session_cost;
                return;
            }
            UiEvent::ToolStarted { name, input, .. } => {
                self.newline_if_needed();
                println!("{DIM}> {name} {}{RESET}", summary(name, input));
            }
            UiEvent::ToolFinished { ok, text, .. } => {
                if !ok {
                    println!("{RED}  failed: {}{RESET}", text.lines().next().unwrap_or(""));
                }
            }
            UiEvent::Retry { attempt, wait, error } => {
                self.newline_if_needed();
                println!("{DIM}retrying in {}s (attempt {attempt}): {error}{RESET}", wait.as_secs());
            }
            UiEvent::Notice(n) => {
                self.newline_if_needed();
                eprintln!("{RED}{n}{RESET}");
            }
            UiEvent::Status(n) => {
                self.newline_if_needed();
                println!("{DIM}{n}{RESET}");
            }
            UiEvent::AutoAllowed { command, risk, reason, .. } => {
                // With parallel tool calls the line does not follow its own call, so it names the command.
                self.newline_if_needed();
                println!("{DIM}  auto: {risk}{}: {command}{RESET}", if reason.is_empty() { String::new() } else { format!(" ({reason})") });
            }
        }
        let _ = std::io::stdout().flush();
    }

    fn ask(&self, p: &PermissionPrompt) -> Reply {
        self.newline_if_needed();
        println!("Permission needed: {} {}", p.action, p.resources.join(", "));
        let always = if p.save.is_empty() { String::new() } else { format!(" ({})", p.save.join(", ")) };
        let answer = self.read_line(&format!("  [y] once  [a] always{always}  [n] reject  or type feedback: "));
        match answer.as_deref().map(str::trim) {
            Some("y") | Some("yes") => Reply::Once,
            Some("a") | Some("always") => Reply::Always,
            None | Some("") | Some("n") | Some("no") => Reply::Reject(None),
            Some(feedback) => Reply::Reject(Some(feedback.to_string())),
        }
    }
}

fn short(id: &str) -> &str {
    &id[id.len().saturating_sub(8)..]
}

fn ago(ms: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let s = ((now - ms) / 1000).max(0);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

fn tilde(dir: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && dir.starts_with(&h) => format!("~{}", &dir[h.len()..]),
        _ => dir.to_string(),
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() > n { format!("{}...", s.chars().take(n - 3).collect::<String>()) } else { s.to_string() }
}

/// Numbered session list; `show_dir` adds the working directory column.
fn print_sessions(list: &[SessionSummary], show_dir: bool) {
    if list.is_empty() {
        println!("{DIM}no sessions{RESET}");
        return;
    }
    for (i, s) in list.iter().enumerate() {
        let title = s.title.as_deref().map(|t| clip(t, 44)).unwrap_or_else(|| "(untitled)".into());
        let dir = if show_dir { format!("  {}", clip(&tilde(&s.directory), 30)) } else { String::new() };
        let mut flags = String::new();
        if s.forked_from.is_some() {
            flags.push_str(" fork");
        }
        if s.archived {
            flags.push_str(" archived");
        }
        println!("{:>3}  {}  {:<44}{dir}  {:>9}  {:>3} msg  ${:.2}  {}{flags}", i + 1, short(&s.id), title, ago(s.time_updated), s.messages, s.cost, s.agent);
    }
}

/// A session from a list number or a piece of its id. Errors are user-facing messages.
fn pick_session(store: &Store, token: &str, last: &[SessionSummary]) -> Result<Session, String> {
    if let (Ok(n), true) = (token.parse::<usize>(), !last.is_empty()) {
        return match last.get(n.wrapping_sub(1)) {
            Some(s) => store.session(&s.id).ok().flatten().ok_or_else(|| "that session no longer exists".to_string()),
            None => Err(format!("no session number {n} in the last list")),
        };
    }
    let mut found = store.find_sessions(token).map_err(|e| e.to_string())?;
    match found.len() {
        0 => Err(format!("no session matches '{token}'")),
        1 => Ok(found.remove(0)),
        _ => Err(format!("'{token}' matches {} sessions; type more of the id", found.len())),
    }
}

/// May a command that a *project's* config defines run? Such a command executes with your permissions, so it needs your
/// approval: asked once in a terminal (`y` this time, `a` always for this project and this exact definition, anything
/// else no) and remembered in the trust store. Without a terminal nobody can be asked: it stays off unless
/// `MOCO_TRUST_PROJECT_CONFIG=1` (or the older `MOCO_TRUST_PROJECT_MCP=1`) is set.
fn project_command_allowed(app_dir: &std::path::Path, origin: Option<&std::path::PathBuf>, what_kind: &str, name: &str, command: &str, fingerprint: &str) -> bool {
    if ["MOCO_TRUST_PROJECT_CONFIG", "MOCO_TRUST_PROJECT_MCP"].iter().any(|v| std::env::var(v).is_ok_and(|x| x == "1")) || trust::is_trusted(app_dir, fingerprint) {
        return true;
    }
    if !std::io::stdin().is_terminal() {
        return false;
    }
    println!("The project config {} wants to run a {what_kind} '{name}':\n  {command}\nIt would run on your machine with your permissions.", origin.map(|o| tilde(&o.display().to_string())).unwrap_or_default());
    print!("Allow it? [y] this time  [a] always for this project  [N] no: ");
    let _ = std::io::stdout().flush();
    let mut ans = String::new();
    let _ = std::io::stdin().lock().read_line(&mut ans);
    match ans.trim().to_lowercase().as_str() {
        "y" | "yes" => true,
        "a" | "always" => {
            let _ = trust::approve(app_dir, fingerprint);
            true
        }
        _ => false,
    }
}

/// The MCP servers to start; servers a project's config defines go through `project_command_allowed`.
fn mcp_specs(cfg: &config::Config, origins: &std::collections::BTreeMap<String, std::path::PathBuf>, app_dir: &std::path::Path, cwd: &std::path::Path) -> (Vec<ServerSpec>, Vec<String>) {
    let (mut specs, mut warnings) = (vec![], vec![]);
    for (name, scfg) in &cfg.mcp.servers {
        let origin = origins.get(name);
        let trusted = scfg.disabled || origin.is_none_or(|o| trust::origin_is_trusted(o, app_dir));
        let what = if scfg.is_remote() { scfg.url.clone().unwrap_or_default() } else { scfg.command.join(" ") };
        if trusted || project_command_allowed(app_dir, origin, "MCP server", name, &what, &trust::fingerprint(cwd, name, scfg)) {
            specs.push(ServerSpec { name: name.clone(), config: scfg.clone(), cwd: cwd.to_path_buf() });
        } else {
            warnings.push(format!("MCP server '{name}' from {} was not started: project-defined commands need your approval (run moco in a terminal, or set MOCO_TRUST_PROJECT_CONFIG=1)", origin.map(|o| tilde(&o.display().to_string())).unwrap_or_default()));
        }
    }
    (specs, warnings)
}

/// The formatters to run after edits: only what the config names, and project-defined ones only if approved.
fn formatters_from(cfg: &config::Config, origins: &std::collections::BTreeMap<String, std::path::PathBuf>, app_dir: &std::path::Path, cwd: &std::path::Path) -> (moco_core::format::Formatters, Vec<String>) {
    let Some(config::FormatterSetting::Formatters(map)) = &cfg.formatter else { return (Default::default(), vec![]) };
    let warnings = std::cell::RefCell::new(vec![]);
    let formatters = moco_core::format::Formatters::from_config(map, &|name| {
        let fc = &map[name];
        let origin = origins.get(name);
        if origin.is_none_or(|o| trust::origin_is_trusted(o, app_dir)) {
            return true;
        }
        let what = format!("{}   (for {})", fc.command.join(" "), fc.extensions.join(" "));
        let ok = project_command_allowed(app_dir, origin, "formatter", name, &what, &trust::fingerprint_formatter(cwd, name, fc));
        if !ok {
            warnings.borrow_mut().push(format!("formatter '{name}' from {} is off: project-defined commands need your approval (run moco in a terminal, or set MOCO_TRUST_PROJECT_CONFIG=1)", origin.map(|o| tilde(&o.display().to_string())).unwrap_or_default()));
        }
        ok
    });
    (formatters, warnings.into_inner())
}

fn print_files(r: &UndoReport) {
    let mut lines: Vec<String> = r.restored.iter().map(|p| format!("  restored {p}")).chain(r.deleted.iter().map(|p| format!("  removed {p}"))).collect();
    let extra = lines.len().saturating_sub(15);
    lines.truncate(15);
    for l in lines {
        println!("{DIM}{l}{RESET}");
    }
    if extra > 0 {
        println!("{DIM}  ... and {extra} more{RESET}");
    }
}

fn die(msg: &str) -> ! {
    eprintln!("moco: {msg}");
    std::process::exit(2)
}

/// Read a secret from stdin; echo is turned off when stdin is a terminal.
fn read_secret(prompt: &str) -> String {
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let tty = std::io::stdin().is_terminal();
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    let echo_off = tty && unsafe { libc::tcgetattr(0, &mut saved) } == 0;
    if echo_off {
        let mut t = saved;
        t.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
    }
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    if echo_off {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
        eprintln!();
    }
    line.trim().to_string()
}

fn price(m: &moco_core::catalog::Model) -> String {
    match &m.cost {
        Some(c) => format!("${}/{} per M", c.input, c.output),
        None => "no price".into(),
    }
}

fn list_models(catalog: &Catalog, app_dir: &std::path::Path, filter: &str, connected_only: bool, limit: usize) -> usize {
    let filter = filter.to_lowercase();
    let mut shown = 0;
    for (pid, p) in &catalog.providers {
        let connected = registry::is_connected(p, app_dir);
        if (connected_only && !connected) || p.protocol().is_none() {
            continue;
        }
        for m in p.models.values() {
            let full = format!("{pid}/{}", m.id);
            if !filter.is_empty() && !full.to_lowercase().contains(&filter) && !m.name.to_lowercase().contains(&filter) {
                continue;
            }
            if shown >= limit {
                return shown + 1; // signals "more"
            }
            println!("{}{full:<48} ctx {:<7} {}", if connected { "* " } else { "  " }, human(m.limit.context), price(m));
            shown += 1;
        }
    }
    shown
}

fn provider_line(p: &ProviderInfo, app_dir: &std::path::Path) -> String {
    let state = if registry::is_connected(p, app_dir) {
        "connected".to_string()
    } else if p.protocol().is_none() {
        "unsupported protocol".to_string()
    } else {
        format!("no key (set {} or run `moco auth {}`)", p.env.first().cloned().unwrap_or_else(|| auth::env_var_for(&p.id)), p.id)
    };
    let experimental = if p.protocol().is_some_and(|x| x.experimental()) { "  [experimental protocol]" } else { "" };
    format!("{:<16} {:>4} models  {state}{experimental}", p.id, p.models.len())
}

/// `moco --eval-classifier FILE`: runs every command in FILE (JSON lines with a `command` field, or plain lines)
/// through the classifier and prints one JSON line per command with the layer that decided and its verdict. The
/// comparison with what was expected is `scripts/eval-classifier.py`. Uses the configured model and records a session
/// in the store, so point `MOCO_HOME` at a scratch directory.
fn eval_classifier_cases(engine: &Engine, file: &str, cwd: &str) {
    let text = std::fs::read_to_string(file).unwrap_or_else(|e| die(&format!("{file}: {e}")));
    let session = engine.store.lock().unwrap().create_session(cwd, "build").unwrap_or_else(|e| die(&format!("database: {e}")));
    let cancel = AtomicBool::new(false);
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let command = match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => v["command"].as_str().map(str::to_string).unwrap_or_else(|| die(&format!("a case has no `command`: {line}"))),
            Err(_) => line.to_string(),
        };
        let ev = engine.evaluate_command(&session, &command, &cancel);
        let (decision, risk, reason) = match ev.verdict {
            moco_core::classifier::Verdict::Allow { risk, reason } => ("allow", risk, reason),
            moco_core::classifier::Verdict::Ask { reason } => ("ask", String::new(), reason),
        };
        println!("{}", serde_json::json!({"command": command, "stage": ev.stage, "decision": decision, "risk": risk, "reason": reason}));
    }
}

fn main() {
    // Let `moco models | head` end quietly instead of panicking on a closed pipe.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let mut raw: Vec<String> = std::env::args().skip(1).collect();
    let app_dir = auth::app_dir();

    // Subcommands that need no session.
    match raw.first().map(String::as_str) {
        Some("auth") => {
            let Some(provider) = raw.get(1) else { die("usage: moco auth <provider>   (reads the key from stdin)") };
            let catalog = Catalog::load(&app_dir);
            if !catalog.providers.contains_key(provider.as_str()) {
                eprintln!("note: '{provider}' is not in the model catalog; storing the key anyway (define it under `providers` in moco.jsonc)");
            }
            let key = read_secret(&format!("API key for {provider}: "));
            if key.is_empty() {
                die("empty key, nothing stored");
            }
            match auth::store_key(provider, &key, &app_dir) {
                Ok(p) => println!("stored in {} (mode 600)", p.display()),
                Err(e) => die(&format!("could not store key: {e}")),
            }
            return;
        }
        Some("sessions") => {
            let store = Store::open(&app_dir.join("data/moco.db")).unwrap_or_else(|e| die(&format!("database: {e}")));
            let rest: Vec<&str> = raw.iter().skip(1).map(String::as_str).collect();
            let need = |what: &str| -> Session { pick_session(&store, rest.get(1).copied().unwrap_or(""), &[]).unwrap_or_else(|e| die(&format!("{what}: {e}"))) };
            match rest.first().copied() {
                Some("rename") => {
                    let ses = need("rename");
                    let title = rest[2..].join(" ");
                    if title.trim().is_empty() {
                        die("usage: moco sessions rename <id> <new title>");
                    }
                    store.set_title(&ses.id, title.trim()).unwrap_or_else(|e| die(&e.to_string()));
                    println!("renamed {}", short(&ses.id));
                }
                Some(cmd @ ("archive" | "unarchive")) => {
                    let ses = need(cmd);
                    store.set_archived(&ses.id, cmd == "archive").unwrap_or_else(|e| die(&e.to_string()));
                    println!("{cmd}d {}", short(&ses.id));
                }
                _ => {
                    let all = rest.contains(&"--all");
                    let cwd = std::env::current_dir().unwrap_or_default().to_string_lossy().into_owned();
                    let list = store.list_sessions(if all { None } else { Some(&cwd) }, rest.contains(&"--archived"), 50).unwrap_or_default();
                    print_sessions(&list, all);
                    if !all && list.is_empty() {
                        println!("{DIM}(only sessions started in {} are shown; use --all for every directory){RESET}", tilde(&cwd));
                    }
                }
            }
            return;
        }
        Some("logs") => {
            let rest: Vec<&str> = raw.iter().skip(1).map(String::as_str).collect();
            let follow = rest.iter().any(|a| matches!(*a, "--follow" | "-f"));
            if rest.contains(&"--path") {
                println!("{}", moco_core::log::log_dir(&app_dir).display());
                return;
            }
            let n = rest.iter().position(|a| matches!(*a, "--lines" | "-n")).and_then(|i| rest.get(i + 1)).map(|v| v.parse::<usize>().unwrap_or_else(|_| die("--lines needs a number"))).unwrap_or(100);
            if let Some(bad) = rest.iter().find(|a| a.starts_with('-') && !matches!(**a, "--follow" | "-f" | "--lines" | "-n" | "--path")) {
                die(&format!("unknown option {bad}; usage: moco logs [--follow] [--lines N] [--path]"));
            }
            match moco_core::log::latest_file(&app_dir) {
                Some(path) => {
                    let text = moco_core::log::read_tail(&path, n).unwrap_or_else(|e| die(&format!("{}: {e}", path.display())));
                    print!("{text}");
                }
                None if !follow => println!("{DIM}(no log yet in {}; set MOCO_LOG=info or debug to record more){RESET}", moco_core::log::log_dir(&app_dir).display()),
                None => {}
            }
            if follow {
                let mut f = moco_core::log::Follow::new(&app_dir);
                loop {
                    let new = f.poll();
                    if !new.is_empty() {
                        print!("{new}");
                        let _ = std::io::stdout().flush();
                    }
                    std::thread::sleep(std::time::Duration::from_millis(400));
                }
            }
            return;
        }
        Some("models") => {
            let refresh = raw.iter().any(|a| a == "--refresh");
            if refresh {
                match Catalog::refresh(&app_dir, "https://models.dev/api.json") {
                    Ok((p, m)) => println!("catalog updated: {p} providers, {m} models"),
                    Err(e) => die(&e),
                }
            }
            let filter = raw.iter().skip(1).find(|a| !a.starts_with("--")).cloned().unwrap_or_default();
            let cwd = std::env::current_dir().unwrap_or_default();
            let mut catalog = Catalog::load(&app_dir);
            catalog.apply_config(&config::load(&app_dir, &cwd).config.providers);
            if filter.is_empty() {
                for p in catalog.providers.values().filter(|p| p.protocol().is_some()) {
                    println!("{}", provider_line(p, &app_dir));
                }
                println!("\n`moco models <filter>` lists models; * marks connected providers.");
            } else if list_models(&catalog, &app_dir, &filter, false, 60) > 60 {
                println!("... more; narrow the filter");
            }
            return;
        }
        _ => {}
    }

    let (mut cont, mut prompt, mut dir, mut session_arg) = (false, None, None, None::<String>);
    let mut want_tui: Option<bool> = None;
    let mut effort_arg: Option<String> = None;
    let mut model = std::env::var("MOCO_MODEL").ok();
    let mut agent_id: Option<String> = None;
    let mut permission_mode_arg: Option<String> = None;
    let mut eval_classifier: Option<String> = None;
    let mut args = raw.drain(..);
    while let Some(a) = args.next() {
        let mut value = |flag: &str| args.next().unwrap_or_else(|| die(&format!("{flag} needs a value")));
        match a.as_str() {
            "--continue" | "-c" => cont = true,
            "--model" | "-m" => model = Some(value("--model")),
            "--agent" => agent_id = Some(value("--agent")),
            "--permission-mode" => permission_mode_arg = Some(value("--permission-mode")),
            "--effort" => effort_arg = Some(value("--effort")),
            "--dir" => dir = Some(value("--dir")),
            "--session" | "-s" => session_arg = Some(value("--session")),
            "--prompt" | "-p" => prompt = Some(value("--prompt")),
            // Not in --help: a development tool for `scripts/eval-classifier.py`.
            "--eval-classifier" => eval_classifier = Some(value("--eval-classifier")),
            "--tui" => want_tui = Some(true),
            "--plain" => want_tui = Some(false),
            "--version" | "-V" => {
                println!("moco {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" | "-h" => {
                println!(
                    "usage: moco [--continue | --session ID] [--model PROVIDER/MODEL] [--agent ID] [--permission-mode manual|auto] [--effort LEVEL] [--dir PATH] [-p PROMPT] [--plain] [--version]\n       moco sessions [--all] [--archived] | rename ID TITLE | archive ID | unarchive ID\n       moco models [FILTER] [--refresh]\n       moco auth PROVIDER\n       moco logs [--follow] [--lines N] [--path]\n\nlogging: MOCO_LOG=error|warn|info|debug|trace (default warn; `warn,mcp=trace` sets one target) or the `log` config key.\n         Files: <data dir>/data/logs/moco-YYYY-MM-DD.log (7 days kept). Nothing is ever printed to the terminal."
                );
                return;
            }
            other => die(&format!("unknown argument {other}")),
        }
    }
    // Only a run that starts a session logs. `--help`, `--version`, `logs`, `models`, `auth` and `sessions` never touch
    // the log directory, so `moco logs` cannot create the file it is about to show.
    moco_core::log::init(&app_dir);
    if let Some(w) = auth::permissions_warning(&app_dir) {
        eprintln!("warning: {w}");
    }
    let store = Store::open(&app_dir.join("data/moco.db")).unwrap_or_else(|e| die(&format!("database: {e}")));
    let chosen: Option<Session> = session_arg.as_deref().map(|tok| pick_session(&store, tok, &[]).unwrap_or_else(|e| die(&e)));
    let cwd = match (&chosen, dir) {
        // A session always runs in the directory it was started in.
        (Some(s), _) => std::path::PathBuf::from(&s.directory).canonicalize().unwrap_or_else(|e| die(&format!("the session's directory {} is not available: {e}", s.directory))),
        (None, Some(d)) => std::path::PathBuf::from(d).canonicalize().unwrap_or_else(|e| die(&format!("--dir: {e}"))),
        (None, None) => std::env::current_dir().unwrap_or_else(|e| die(&e.to_string())),
    };
    let cwd_str = cwd.to_string_lossy().into_owned();
    std::env::set_current_dir(&cwd).ok();

    let loaded = config::load(&app_dir, &cwd);
    for w in &loaded.warnings {
        eprintln!("warning: config: {w}");
    }
    let cfg = loaded.config;
    moco_core::log::configure(cfg.log.as_deref());
    moco_core::log_info!("main", "start dir={} sources={}", cwd_str, loaded.sources.len());
    if let (Some(sh), None) = (&cfg.shell, std::env::var_os("MOCO_SHELL")) {
        std::env::set_var("MOCO_SHELL", sh);
    }
    // The flag applies to this session and is the default for new ones; otherwise the config key is the default (manual).
    let flag_mode = permission_mode_arg.as_deref().map(|s| permission::PermissionMode::parse(s).unwrap_or_else(|| die(&format!("--permission-mode must be manual or auto, not {s}"))));
    let default_mode = match flag_mode {
        Some(m) => m,
        None => config::permission_mode(&cfg).unwrap_or_else(|e| die(&e)),
    };
    config::preset_rules(&cfg).unwrap_or_else(|e| die(&e));
    if let Some(p) = &cfg.permission_preset {
        eprintln!("permission preset: {p}");
    }
    moco_core::sandbox::configure(&cfg.sandbox);
    if let Some(w) = moco_core::sandbox::unavailable_notice(&cfg.sandbox) {
        eprintln!("warning: {w}");
    }
    let mode_agents: std::collections::BTreeMap<_, _> = [permission::PermissionMode::Manual, permission::PermissionMode::Auto].into_iter().map(|m| (m, config::resolve_agents_in(&cfg, m))).collect();
    let agents = mode_agents[&default_mode].clone();
    let agent_explicit = agent_id.is_some();
    let mut agent_id = agent_id.or(cfg.default_agent.clone()).unwrap_or_else(|| "build".into());
    if !agents.iter().any(|a| a.id == agent_id && a.selectable()) {
        die(&format!("unknown agent {agent_id}; available: {}", agents.iter().filter(|a| a.selectable()).map(|a| a.id.as_str()).collect::<Vec<_>>().join(", ")));
    }
    let model = model.or(cfg.model.clone()).unwrap_or_else(|| "anthropic/claude-sonnet-5-5".into());

    let mut catalog = Catalog::load(&app_dir);
    catalog.apply_config(&cfg.providers);
    let providers = registry::build(&catalog, &app_dir);
    let (want_provider, _) = config::split_model(&model);
    if !providers.contains_key(want_provider) && !agents.iter().any(|a| a.model.is_some()) {
        let mut have: Vec<&str> = providers.keys().map(String::as_str).collect();
        have.sort();
        die(&format!(
            "provider '{want_provider}' is not connected. Set {} or run `moco auth {want_provider}`. Connected: {}. Use --model PROVIDER/MODEL to pick another (see `moco models`).",
            auth::env_var_for(want_provider),
            if have.is_empty() { "none".to_string() } else { have.join(", ") }
        ));
    }

    let recovered = store.recover_claims(3).unwrap_or_default();
    let resumed = chosen.is_some() || cont;
    let mut session = match chosen.or_else(|| cont.then(|| store.latest_session(&cwd_str).ok().flatten()).flatten()) {
        Some(s) => s,
        None => store.create_session(&cwd_str, &agent_id).unwrap_or_else(|e| die(&e.to_string())),
    };
    if agent_explicit {
        store.set_agent(&session.id, &agent_id).ok();
    } else if resumed && agents.iter().any(|a| a.id == session.agent && a.selectable()) {
        agent_id = session.agent.clone(); // a resumed session keeps its own agent
    }
    if let Some(m) = flag_mode {
        store.set_permission_mode(&session.id, Some(m.name())).ok();
        session.permission_mode = Some(m.name().into());
    }
    // An explicit --model / MOCO_MODEL applies to this session; otherwise a resumed session keeps its own.
    let explicit_model = std::env::args().any(|a| a == "--model" || a == "-m") || std::env::var("MOCO_MODEL").is_ok();
    if explicit_model {
        store.set_model(&session.id, Some(&model)).ok();
        session.model = Some(model.clone());
    }

    if let Some(e) = &effort_arg {
        store.set_variant(&session.id, Some(e)).ok();
    }
    let mut engine_cfg = EngineConfig::new(&model, app_dir.clone());
    engine_cfg.effort = cfg.effort.clone();
    engine_cfg.agents = agents;
    engine_cfg.permission_mode = default_mode;
    engine_cfg.agents_by_mode = mode_agents;
    engine_cfg.instructions = cfg.instructions.clone();
    engine_cfg.catalog = catalog;
    engine_cfg.compaction = compaction::Settings::from_config(&cfg.compaction);
    engine_cfg.snapshots = cfg.snapshots.unwrap_or(true);
    let found_skills = skills::discover(&app_dir, &cwd, &cfg.skills);
    let found_commands = commands::discover(&app_dir, &cwd, &cfg.commands);
    engine_cfg.skills = found_skills.clone();
    engine_cfg.small_model = cfg.small_model.clone();
    engine_cfg.classifier = loaded.classifier.settings();
    let (mcp_specs, mcp_warnings) = mcp_specs(&cfg, &loaded.mcp_origins, &app_dir, &cwd);
    for w in &mcp_warnings {
        eprintln!("warning: {w}");
    }
    let manager: Option<std::sync::Arc<Manager>> = (!mcp_specs.is_empty()).then(|| Manager::start(mcp_specs, &cfg.mcp.timeout, &app_dir));
    if let Some(m) = &manager {
        // Tools are fixed once a conversation starts, so wait for the servers instead of letting them appear mid-way.
        let wait = std::time::Duration::from_millis(cfg.mcp.timeout.startup.unwrap_or(30_000) + 2_000);
        if !m.wait_ready(std::time::Duration::from_millis(1_500)) {
            eprintln!("{DIM}starting MCP servers...{RESET}");
            m.wait_ready(wait);
        }
        for info in m.status() {
            if !matches!(info.status, mcp::Status::Connected | mcp::Status::Disabled) {
                let tail = m.stderr_tail(&info.name);
                eprintln!("warning: MCP server '{}' {}{}", info.name, mcp::manager::describe(&info), if tail.is_empty() { String::new() } else { format!(" (stderr: {tail})") });
            }
        }
    }
    engine_cfg.mcp = manager.clone();
    let (formatters, formatter_warnings) = formatters_from(&cfg, &loaded.formatter_origins, &app_dir, &cwd);
    for w in &formatter_warnings {
        eprintln!("warning: {w}");
    }
    engine_cfg.formatters = formatters;
    engine_cfg.websearch = moco_core::tools::websearch::Settings::from_config(&cfg.websearch, app_dir.clone());
    let n_sources = loaded.sources.len();
    let engine = Engine::with_providers(store, providers, engine_cfg).share();
    if let Some(cases) = eval_classifier {
        return eval_classifier_cases(&engine, &cases, &cwd_str);
    }
    engine.set_notifier(std::sync::Arc::new(|line| {
        if AT_PROMPT.load(Ordering::Relaxed) {
            // Redraw the prompt line underneath the notice.
            print!("\r\x1b[2K{DIM}[{line}]{RESET}\n> ");
        } else {
            println!("\n{DIM}[{line}]{RESET}");
        }
        let _ = std::io::stdout().flush();
    }));
    // Drop snapshot data that no session active in the last 30 days refers to.
    let projects = engine.store.lock().unwrap().project_ids().unwrap_or_default();
    engine.prune_snapshots(&projects, std::time::Duration::from_secs(30 * 24 * 3600));

    install_sigint();
    let term = Terminal { mid_line: AtomicBool::new(false), run: Mutex::new(RunUsage::default()) };
    let run = |sid: &str, resume: bool| {
        CANCEL.store(false, Ordering::Relaxed);
        let outcome = engine.run(sid, &term, &CANCEL, resume);
        term.newline_if_needed();
        match &outcome {
            Ok(RunOutcome::Idle) => {}
            Ok(RunOutcome::Interrupted) => println!("{DIM}interrupted{RESET}"),
            Ok(RunOutcome::Declined) => println!("{DIM}stopped: tool call rejected{RESET}"),
            Ok(RunOutcome::Failed(_)) => {} // already reported via Notice
            Ok(RunOutcome::Busy) => eprintln!("session is busy"),
            Err(e) => eprintln!("{RED}store error: {e}{RESET}"),
        }
        term.print_usage();
        outcome.ok()
    };

    // Expand @mentions of files, directories, skills and agents, tell the user what was attached, then queue the prompt.
    let caps_of = |sid: &str| engine.store.lock().unwrap().session(sid).ok().flatten().map(|s| engine.current_media_caps(&s)).unwrap_or_default();
    let send = |sid: &str, text: &str| -> bool {
        let subagents = engine.subagent_names();
        let caps = caps_of(sid);
        let (m, notes) = mentions::expand(text, &mentions::Ctx { cwd: &cwd, skills: &found_skills, subagents: &subagents, app_dir: &app_dir, caps, mcp: manager.as_deref() });
        for n in notes {
            println!("{DIM}{n}{RESET}");
        }
        engine.submit_with(sid, text, Some(m), Mode::Steer).is_ok()
    };

    let on_terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if want_tui == Some(true) && !on_terminal {
        die("--tui needs a terminal on stdin and stdout");
    }
    // The full-screen UI is the default on a terminal; -p, pipes and --plain keep the line-based chat.
    if prompt.is_none() && want_tui.unwrap_or(on_terminal) {
        let sid = session.id.clone();
        let submit = |text: &str, mode: Mode| -> Result<(String, Vec<String>), String> {
            let subagents = engine.subagent_names();
            let caps = caps_of(&sid);
            let (m, notes) = mentions::expand(text, &mentions::Ctx { cwd: &cwd, skills: &found_skills, subagents: &subagents, app_dir: &app_dir, caps, mcp: manager.as_deref() });
            let id = engine.submit_with(&sid, text, Some(m), mode).map_err(|e| format!("could not queue the prompt: {e}"))?;
            Ok((id, notes))
        };
        let command = |line: &str| -> Result<tui::Resolved, String> {
            let (name, args) = line[1..].split_once(char::is_whitespace).unwrap_or((&line[1..], ""));
            if let Some(m) = manager.as_ref().filter(|m| m.prompts().iter().any(|p| p.command == name)) {
                return m.get_prompt(name, args).map(|text| tui::Resolved { text, agent: None, model: None });
            }
            let cmd = found_commands.iter().find(|c| c.name == name).ok_or_else(|| format!("unknown command /{name} (Ctrl-P lists the commands)"))?;
            if cmd.subagent {
                return Err(format!("/{name} runs in a subagent, which the full-screen UI cannot show yet; run `moco --plain` for it"));
            }
            Ok(tui::Resolved { text: commands::expand(cmd, args), agent: cmd.agent.clone(), model: cmd.model.clone() })
        };
        let completer = tui::Completer::new(
            &cwd,
            found_skills.iter().map(|k| (k.id.clone(), k.description.clone())).collect(),
            engine.agents().iter().filter(|a| a.subagent()).map(|a| (a.id.clone(), a.description.clone())).collect(),
            found_commands.iter().filter(|c| !c.subagent).map(|c| (c.name.clone(), c.description.clone())).collect(),
            manager.clone(),
        );
        let title = engine.store.lock().unwrap().session(&sid).ok().flatten();
        let model_now = title.as_ref().map(|s| engine.model_ref_for(s)).unwrap_or_default();
        let host = tui::Host { engine: &engine, session_id: sid.clone(), model: model_now, agent: agent_id.clone(), title: title.and_then(|s| s.title), recovered: recovered.contains(&sid), submit: &submit, command: &command, completer: Some(completer), history_path: app_dir.join("data/tui-history"), cwd: cwd_str.clone(), skills: found_skills.iter().map(|k| (k.id.clone(), k.description.clone())).collect(), custom_commands: found_commands.iter().map(|c| (c.name.clone(), c.description.clone())).collect(), manager: manager.clone(), app_dir: app_dir.clone(), tui: cfg.tui.clone() };
        let result = tui::run(host);
        engine.kill_all_jobs();
        if let Some(m) = &manager {
            m.shutdown();
        }
        if let Err(e) = result {
            die(&format!("terminal error: {e}"));
        }
        return;
    }

    if recovered.contains(&session.id) {
        println!("{DIM}resuming interrupted run{RESET}");
        run(&session.id, true);
    }
    if let Some(p) = prompt {
        // `-p "/review the last commit"` works like typing the command.
        let text = match p.strip_prefix('/').and_then(|r| {
            let (name, args) = r.split_once(char::is_whitespace).unwrap_or((r, ""));
            if let Some(m) = manager.as_ref().filter(|m| m.prompts().iter().any(|p| p.command == name)) {
                return Some(m.get_prompt(name, args).unwrap_or_else(|e| die(&e)));
            }
            found_commands.iter().find(|c| c.name == name && !c.subagent).map(|c| commands::expand(c, args))
        }) {
            Some(t) => t,
            None => p,
        };
        if !send(&session.id, &text) {
            die("could not queue the prompt");
        }
        let mut ok = matches!(run(&session.id, false), Some(RunOutcome::Idle));
        // Background jobs started during the run: wait for them and let the assistant report what they produced.
        while ok && !CANCEL.load(Ordering::Relaxed) {
            if engine.running_jobs() > 0 {
                eprintln!("{DIM}waiting for {} background job(s)... (Ctrl-C stops them){RESET}", engine.running_jobs());
                engine.wait_for_jobs(&CANCEL, std::time::Duration::from_secs(6 * 3600));
            }
            if CANCEL.load(Ordering::Relaxed) || engine.store.lock().unwrap().pending_inbox(&session.id).unwrap_or(0) == 0 {
                break;
            }
            ok = matches!(run(&session.id, false), Some(RunOutcome::Idle));
        }
        engine.kill_all_jobs();
        if let Some(m) = &manager {
            m.shutdown(); // process::exit skips destructors, and the servers must not be left running
        }
        std::process::exit(if ok { 0 } else { 1 });
    }

    let title_of = |sid: &str| engine.store.lock().unwrap().session(sid).ok().flatten().and_then(|s| s.title);
    let current = |sid: &str| engine.store.lock().unwrap().session(sid).ok().flatten().map(|s| engine.model_ref_for(&s)).unwrap_or_default();
    println!("{DIM}moco {} | {} | agent {agent_id} | mode {} | {n_sources} config file(s) | /help for commands{RESET}", env!("CARGO_PKG_VERSION"), current(&session.id), engine.permission_mode_of(&session).name());
    if resumed {
        println!("{DIM}session {}: {}{RESET}", short(&session.id), title_of(&session.id).unwrap_or_else(|| "(untitled)".into()));
    }
    let mut last_list: Vec<SessionSummary> = vec![];
    loop {
        AT_PROMPT.store(true, Ordering::Relaxed);
        let line = term.read_line("> ");
        AT_PROMPT.store(false, Ordering::Relaxed);
        let Some(line) = line else { break };
        CANCEL.store(false, Ordering::Relaxed);
        let line = line.trim();
        match line {
            "" => {
                // Enter on an empty line lets the assistant react to background results that arrived while you were idle.
                if engine.store.lock().unwrap().pending_inbox(&session.id).unwrap_or(0) > 0 {
                    run(&session.id, false);
                }
                continue;
            }
            "/exit" | "/quit" => break,
            "/skills" => {
                if found_skills.is_empty() {
                    println!("{DIM}no skills found (put SKILL.md folders in .moco/skills, .claude/skills or the config skills dir){RESET}");
                }
                for k in &found_skills {
                    println!("{DIM}  {:<20} {}{RESET}", k.id, clip(&k.description, 90));
                }
            }
            l if l.starts_with('!') => {
                let command = l[1..].trim();
                if command.is_empty() {
                    println!("usage: !<shell command>   runs it now in {}; the assistant is told what you ran", tilde(&cwd_str));
                    continue;
                }
                CANCEL.store(false, Ordering::Relaxed);
                match engine.run_user_command(&session.id, command, &CANCEL) {
                    Ok(out) => println!("{}", out.trim_end()),
                    Err(e) => eprintln!("{RED}{e}{RESET}"),
                }
            }
            "/permissions" => match engine.permissions_report(&session.id) {
                Ok(lines) => {
                    for l in lines {
                        println!("{DIM}{l}{RESET}");
                    }
                }
                Err(e) => eprintln!("{RED}{e}{RESET}"),
            },
            l if l == "/jobs" || l.starts_with("/jobs ") => {
                let arg = l.trim_start_matches("/jobs").trim();
                if let Some(id) = arg.strip_prefix("output") {
                    match engine.job_output(id.trim(), 40) {
                        Ok(t) => println!("{t}"),
                        Err(e) => println!("{e}"),
                    }
                } else if let Some(id) = arg.strip_prefix("kill") {
                    match engine.kill_job(id.trim()) {
                        Ok(t) => println!("{DIM}{t}{RESET}"),
                        Err(e) => println!("{e}"),
                    }
                } else {
                    let jobs = engine.background_jobs(None);
                    if jobs.is_empty() {
                        println!("{DIM}no background jobs{RESET}");
                    }
                    for j in jobs {
                        println!("{DIM}  {}  {:<8} {:<18} {:>5}s  {}{RESET}", j.id, j.kind, j.status, j.seconds, clip(j.title.lines().next().unwrap_or(""), 60));
                    }
                    println!("{DIM}  /jobs output <id>, /jobs kill <id>{RESET}");
                }
            }
            l if l == "/mcp" || l.starts_with("/mcp ") => {
                let Some(m) = &manager else {
                    println!("{DIM}no MCP servers configured (add `mcp.servers` to moco.jsonc){RESET}");
                    continue;
                };
                let arg = l.trim_start_matches("/mcp").trim();
                if let Some(name) = arg.strip_prefix("auth") {
                    let name = name.trim();
                    CANCEL.store(false, Ordering::Relaxed);
                    let show = |url: &str| println!("\nSign in to '{name}' by opening this address in a browser:\n\n  {url}\n");
                    let open = |url: &str| {
                        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
                        let _ = std::process::Command::new(opener).arg(url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
                    };
                    let paste = || {
                        let line = term.read_line("If your browser is on another machine, paste the address it ended up on here (it starts with http://127.0.0.1). Otherwise press Enter and finish in the browser: ")?;
                        (!line.trim().is_empty()).then(|| line.trim().to_string())
                    };
                    let io = LoginIo { show: &show, open: &open, paste: &paste, cancel: &CANCEL };
                    match m.authorize(name, &io) {
                        Ok(()) => println!("{DIM}signed in to '{name}'{RESET}"),
                        Err(e) => println!("{RED}sign-in failed: {e}{RESET}"),
                    }
                } else if let Some(name) = arg.strip_prefix("logout") {
                    match m.logout(name.trim()) {
                        Ok(true) => println!("{DIM}forgot the stored sign-in for '{}'{RESET}", name.trim()),
                        Ok(false) => println!("{DIM}no stored sign-in for '{}'{RESET}", name.trim()),
                        Err(e) => println!("{e}"),
                    }
                    m.wait_ready(std::time::Duration::from_secs(5));
                } else if let Some(name) = arg.strip_prefix("reconnect") {
                    let name = name.trim();
                    if !m.reconnect(name) {
                        println!("no enabled MCP server '{name}'");
                    } else {
                        m.wait_ready(std::time::Duration::from_secs(35));
                    }
                }
                for i in m.status() {
                    println!("{DIM}  {:<14} {}   [{}]{RESET}", i.name, mcp::manager::describe(&i), clip(&i.command, 50));
                }
                println!("{DIM}  tools from MCP servers are named <server>_<tool>; /mcp reconnect <name> restarts one; /mcp auth <name> signs in to a remote one{RESET}");
            }
            "/commands" => {
                if let Some(m) = &manager {
                    for p in m.prompts() {
                        println!("{DIM}  /{:<18} {}  [mcp]{RESET}", p.command, clip(&p.description, 70));
                    }
                }
                for c in &found_commands {
                    println!("{DIM}  /{:<18} {}{}{RESET}", c.name, clip(&c.description, 80), if c.source == "built-in" { String::new() } else { format!("  [{}]", if c.source == "config" { "config".to_string() } else { tilde(&c.source) }) });
                }
            }
            "/help" => println!(
                "/model [FILTER|PROVIDER/MODEL]  show, search or switch the model\n/agent <id>                     switch agent\n/mode [manual|auto]             ask before acting (manual, the default) or run tools without asking (auto)\n/effort [level|default]          reasoning effort for models that offer levels\n/compact                        summarize older messages to free up context\n/sessions [all|archived]       list sessions (/resume <n|id> to continue one)\n/rename <title>                 name this session\n/fork [N]                       copy this conversation into a new session\n/archive [n|id]                 hide a session from the list (/unarchive to restore)\n/undo, /redo                    undo the last turn's file changes and messages, or bring them back\n/diff                           changes in the working tree since this session started\n/new                            start a new session\n/jobs                           background commands and subagents (output <id>, kill <id>)\n/permissions                    what the classifier allowed automatically, and the saved always-allow rules\n!<command>                      run a shell command yourself; the assistant is told\n/skills, /commands, /mcp       list skills, custom slash commands (also MCP prompts) and MCP servers (@name mentions a skill, agent or file)\n/exit                           quit (Ctrl-C interrupts a running turn)"
            ),
            l if l == "/sessions" || l.starts_with("/sessions ") => {
                let which = l.trim_start_matches("/sessions").trim();
                let (all, archived) = (which == "all", which == "archived");
                let list = engine.store.lock().unwrap().list_sessions(if all { None } else { Some(&cwd_str) }, archived, 30).unwrap_or_default();
                print_sessions(&list, all);
                if !list.is_empty() {
                    println!("{DIM}/resume <number|id> to continue one{}{RESET}", if archived { ", /unarchive <number|id> to restore it" } else { "" });
                }
                last_list = list;
            }
            l if l.starts_with("/resume") => {
                let tok = l.trim_start_matches("/resume").trim();
                let picked = pick_session(&engine.store.lock().unwrap(), tok, &last_list);
                match picked {
                    _ if tok.is_empty() => println!("usage: /resume <number from /sessions | piece of a session id>"),
                    Err(e) => println!("{e}"),
                    Ok(s) if s.directory != cwd_str => println!("that session belongs to {}; start moco there, or use `moco --session {}`", tilde(&s.directory), short(&s.id)),
                    Ok(s) => {
                        agent_id = if engine.agents().iter().any(|a| a.id == s.agent && a.selectable()) { s.agent.clone() } else { agent_id };
                        println!("{DIM}session {}: {}{RESET}", short(&s.id), s.title.clone().unwrap_or_else(|| "(untitled)".into()));
                        session = s;
                    }
                }
            }
            l if l.starts_with("/rename") => {
                let t = l.trim_start_matches("/rename").trim();
                if t.is_empty() {
                    println!("usage: /rename <new title>");
                } else if engine.store.lock().unwrap().set_title(&session.id, t).is_ok() {
                    println!("{DIM}renamed{RESET}");
                }
            }
            l if l.starts_with("/fork") => {
                let arg = l.trim_start_matches("/fork").trim();
                match arg.parse::<usize>().or_else(|_| if arg.is_empty() { Ok(0) } else { Err(()) }) {
                    Err(()) => println!("usage: /fork [N]   (N = branch off before the Nth most recent prompt; no N copies everything)"),
                    Ok(n) => match engine.fork(&session.id, n) {
                        Ok(Some(f)) => {
                            println!("{DIM}forked into session {}: {}{RESET}", short(&f.id), f.title.clone().unwrap_or_else(|| "(untitled)".into()));
                            session = f;
                        }
                        Ok(None) => println!("this session has fewer than {n} prompt(s)"),
                        Err(e) => eprintln!("{RED}store error: {e}{RESET}"),
                    },
                }
            }
            l if l.starts_with("/archive") || l.starts_with("/unarchive") => {
                let unarchive = l.starts_with("/unarchive");
                let tok = l.trim_start_matches("/unarchive").trim_start_matches("/archive").trim();
                let target = if tok.is_empty() && !unarchive { Ok(session.clone()) } else { pick_session(&engine.store.lock().unwrap(), tok, &last_list) };
                match target {
                    Err(e) => println!("{e}"),
                    Ok(t) => {
                        engine.store.lock().unwrap().set_archived(&t.id, !unarchive).ok();
                        println!("{DIM}{} {}{RESET}", if unarchive { "unarchived" } else { "archived" }, short(&t.id));
                        if !unarchive && t.id == session.id {
                            match engine.store.lock().unwrap().create_session(&cwd_str, &agent_id) {
                                Ok(n) => {
                                    session = n;
                                    println!("{DIM}started a new session{RESET}");
                                }
                                Err(e) => eprintln!("{e}"),
                            }
                        }
                    }
                }
            }
            "/undo" => match engine.undo(&session.id) {
                Ok(UndoResult::Done(r)) => {
                    println!("{DIM}undone: {} message(s) hidden{}{RESET}", r.hidden_messages, if r.files_restored { format!(", {} file(s) restored, {} removed", r.restored.len(), r.deleted.len()) } else { "; no file snapshot for this turn, so only the conversation was rolled back".into() });
                    print_files(&r);
                    println!("{DIM}/undo goes further back, /redo brings it all back, sending a message makes the undo permanent{RESET}");
                }
                Ok(UndoResult::NothingToUndo) => println!("{DIM}nothing to undo{RESET}"),
                Ok(UndoResult::Busy) => eprintln!("session is busy"),
                Ok(UndoResult::Failed(e)) => eprintln!("{RED}undo failed: {e}{RESET}"),
                Err(e) => eprintln!("{RED}store error: {e}{RESET}"),
            },
            "/redo" => match engine.redo(&session.id) {
                Ok(UndoResult::Done(r)) => {
                    println!("{DIM}redone: {} file(s) restored, {} removed{RESET}", r.restored.len(), r.deleted.len());
                    print_files(&r);
                }
                Ok(UndoResult::NothingToUndo) => println!("{DIM}nothing to redo{RESET}"),
                Ok(UndoResult::Busy) => eprintln!("session is busy"),
                Ok(UndoResult::Failed(e)) => eprintln!("{RED}redo failed: {e}{RESET}"),
                Err(e) => eprintln!("{RED}store error: {e}{RESET}"),
            },
            "/diff" => match engine.session_diff(&session.id) {
                Ok(DiffResult::Files(d)) if d.is_empty() => println!("{DIM}no changes since the session started{RESET}"),
                Ok(DiffResult::Files(d)) => {
                    let (a, r): (usize, usize) = d.iter().fold((0, 0), |(a, r), f| (a + f.additions, r + f.deletions));
                    println!("{DIM}{} file(s) changed, +{a} -{r}{RESET}", d.len());
                    for f in &d {
                        if f.binary {
                            println!("{DIM}Binary file {} changed{RESET}", f.path);
                        }
                        for l in f.patch.lines() {
                            match l.chars().next() {
                                Some('+') if !l.starts_with("+++") => println!("\x1b[32m{l}{RESET}"),
                                Some('-') if !l.starts_with("---") => println!("{RED}{l}{RESET}"),
                                _ => println!("{DIM}{l}{RESET}"),
                            }
                        }
                    }
                }
                Ok(DiffResult::NoSnapshots) => println!("{DIM}no snapshots yet: nothing has run in this session{RESET}"),
                Ok(DiffResult::Failed(e)) => eprintln!("{RED}diff failed: {e}{RESET}"),
                Err(e) => eprintln!("{RED}store error: {e}{RESET}"),
            },
            "/compact" => {
                CANCEL.store(false, Ordering::Relaxed);
                match engine.compact(&session.id, &term, &CANCEL) {
                    Ok(CompactResult::Compacted { .. }) => {}
                    Ok(CompactResult::NothingToCompact) => println!("{DIM}nothing to compact yet (recent messages are kept as they are){RESET}"),
                    Ok(CompactResult::Interrupted) => println!("{DIM}interrupted{RESET}"),
                    Ok(CompactResult::Failed(e)) => eprintln!("{RED}compaction failed: {e}{RESET}"),
                    Err(e) => eprintln!("{RED}store error: {e}{RESET}"),
                }
            }
            "/new" => match engine.store.lock().unwrap().create_session(&cwd_str, &agent_id) {
                Ok(s) => {
                    session = s;
                    println!("{DIM}new session{RESET}");
                }
                Err(e) => eprintln!("{e}"),
            },
            l if l.starts_with("/model") => {
                let arg = l.trim_start_matches("/model").trim();
                let connected = engine.connected_providers();
                if arg.is_empty() {
                    println!("{DIM}model: {}{RESET}\nconnected: {}\n/model <filter> searches; /model provider/model switches", current(&session.id), connected.join(", "));
                } else if let Some((pid, mid)) = arg.split_once('/').filter(|(p, _)| connected.iter().any(|c| c == p)) {
                    let known = engine.catalog().model(pid, mid).is_some();
                    engine.store.lock().unwrap().set_model(&session.id, Some(arg)).ok();
                    println!("{DIM}model: {arg}{}{RESET}", if known { "" } else { " (not in the catalog: no price or context limit)" });
                } else if arg.contains('/') && !arg.contains(' ') && engine.catalog().providers.contains_key(arg.split('/').next().unwrap_or("")) {
                    println!("provider '{}' is not connected; run `moco auth {}`", arg.split('/').next().unwrap_or(""), arg.split('/').next().unwrap_or(""));
                } else {
                    let n = list_models(engine.catalog(), &app_dir, arg, true, 25);
                    if n == 0 {
                        println!("no connected model matches '{arg}'");
                    } else if n > 25 {
                        println!("... more; narrow the filter");
                    }
                }
            }
            l if l == "/effort" || l.starts_with("/effort ") => {
                let arg = l.trim_start_matches("/effort").trim();
                let Some(s) = engine.store.lock().unwrap().session(&session.id).ok().flatten() else { continue };
                let info = engine.effort_info(&s);
                if info.levels.is_empty() {
                    println!("{DIM}{} has no reasoning levels to choose from{RESET}", current(&session.id));
                } else if arg.is_empty() {
                    println!("{DIM}reasoning effort: {}{}\nlevels: {}\n/effort <level> sets it, /effort default goes back to the provider's default{RESET}", info.desired.as_deref().unwrap_or("default"), if info.desired.is_some() && !info.offered { " (this model does not offer it, so it is not sent)" } else { "" }, info.levels.join(", "));
                } else if arg == "default" {
                    engine.store.lock().unwrap().set_variant(&session.id, None).ok();
                    println!("{DIM}reasoning effort: default{RESET}");
                } else if info.levels.iter().any(|x| x == arg) {
                    engine.store.lock().unwrap().set_variant(&session.id, Some(arg)).ok();
                    println!("{DIM}reasoning effort: {arg}{RESET}");
                } else {
                    println!("this model offers: {}", info.levels.join(", "));
                }
            }
            l if l == "/mode" || l.starts_with("/mode ") => {
                let want = l.trim_start_matches("/mode").trim();
                let fresh = engine.store.lock().unwrap().session(&session.id).ok().flatten().unwrap_or_else(|| session.clone());
                let current = engine.permission_mode_of(&fresh);
                if want.is_empty() {
                    println!("{DIM}permission mode: {} ({}){RESET}", current.name(), current.describe());
                } else if let Some(m) = permission::PermissionMode::parse(want) {
                    engine.store.lock().unwrap().set_permission_mode(&session.id, Some(m.name())).ok();
                    println!("{DIM}permission mode: {} ({}){RESET}", m.name(), m.describe());
                } else {
                    println!("usage: /mode [manual|auto]");
                }
            }
            l if l.starts_with("/agent") => {
                let want = l.trim_start_matches("/agent").trim();
                match engine.agents().iter().find(|a| a.id == want && a.selectable()) {
                    Some(a) => {
                        agent_id = a.id.clone();
                        engine.store.lock().unwrap().set_agent(&session.id, &a.id).ok();
                        println!("{DIM}agent: {}{RESET}", a.name);
                    }
                    None => println!("unknown agent; available: {}", engine.agents().iter().filter(|a| a.selectable()).map(|a| a.id.as_str()).collect::<Vec<_>>().join(", ")),
                }
            }
            l if l.starts_with('/') => {
                let (name, args) = l[1..].split_once(char::is_whitespace).unwrap_or((&l[1..], ""));
                if let Some(m) = manager.as_ref().filter(|m| m.prompts().iter().any(|p| p.command == name)) {
                    match m.get_prompt(name, args) {
                        Ok(text) => {
                            if send(&session.id, &text) {
                                run(&session.id, false);
                            }
                        }
                        Err(e) => println!("{e}"),
                    }
                    continue;
                }
                let Some(cmd) = found_commands.iter().find(|c| c.name == name) else {
                    println!("unknown command /{name} (try /help, /commands)");
                    continue;
                };
                let prompt = commands::expand(cmd, args);
                if cmd.subagent {
                    // Runs in a child session; the result is shown here and recorded in this conversation.
                    let parent = engine.store.lock().unwrap().session(&session.id).ok().flatten();
                    let Some(parent) = parent else { continue };
                    let subagents = engine.subagent_names();
                    let caps = caps_of(&session.id);
                    let (m, notes) = mentions::expand(&prompt, &mentions::Ctx { cwd: &cwd, skills: &found_skills, subagents: &subagents, app_dir: &app_dir, caps, mcp: manager.as_deref() });
                    for n in notes {
                        println!("{DIM}{n}{RESET}");
                    }
                    let full = std::iter::once(prompt.clone()).chain(mentions::render(&m)).collect::<Vec<_>>().join("\n\n");
                    let req = SubagentRequest { agent: cmd.agent.clone().unwrap_or_else(|| "general".into()), description: cmd.name.clone(), prompt: full, model: cmd.model.clone(), session_id: None };
                    CANCEL.store(false, Ordering::Relaxed);
                    match engine.run_subagent(&parent, req, &term, &CANCEL) {
                        Ok(r) => {
                            println!("{}", r.output);
                            let st = engine.store.lock().unwrap();
                            st.add_message(&session.id, "synthetic", &serde_json::json!({"text": format!("Result of /{} {}:\n\n{}", cmd.name, args.trim(), r.output)})).ok();
                        }
                        Err(e) => eprintln!("{RED}/{} failed: {}{RESET}", cmd.name, e.message()),
                    }
                    // Subagent events carry the child's totals; show this conversation's (children roll up into it).
                    if let Ok((total, _)) = engine.store.lock().unwrap().usage_totals(&session.id) {
                        term.run.lock().unwrap().session_cost = total;
                    }
                    term.print_usage();
                } else {
                    // `agent` and `model` in the command apply to this one prompt only.
                    let (saved_agent, saved_model) = {
                        let st = engine.store.lock().unwrap();
                        let cur = st.session(&session.id).ok().flatten();
                        if let Some(a) = &cmd.agent {
                            if engine.agents().iter().any(|x| x.id == *a) {
                                st.set_agent(&session.id, a).ok();
                            } else {
                                println!("unknown agent '{a}' in /{name}; using the current one");
                            }
                        }
                        if let Some(m) = &cmd.model {
                            st.set_model(&session.id, Some(m)).ok();
                        }
                        (cur.as_ref().map(|c| c.agent.clone()), cur.and_then(|c| c.model))
                    };
                    if send(&session.id, &prompt) {
                        run(&session.id, false);
                    }
                    let st = engine.store.lock().unwrap();
                    if let Some(a) = saved_agent {
                        st.set_agent(&session.id, &a).ok();
                    }
                    if cmd.model.is_some() {
                        st.set_model(&session.id, saved_model.as_deref()).ok();
                    }
                }
            }
            _ => {
                if send(&session.id, line) {
                    run(&session.id, false);
                }
            }
        }
    }
    let stopped = engine.kill_all_jobs();
    if stopped > 0 {
        println!("{DIM}stopped {stopped} background job(s){RESET}");
    }
    if let Some(m) = &manager {
        m.shutdown();
    }
}
