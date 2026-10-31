//! moco: line-based terminal chat (Phase 1 UI).
use moco_core::engine::{Engine, EngineConfig, PermissionPrompt, Reply, RunOutcome, Ui, UiEvent};
use moco_core::provider::anthropic::Anthropic;
use moco_core::store::{Mode, Store};
use moco_core::{agent, auth};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};

static CANCEL: AtomicBool = AtomicBool::new(false);

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

struct Terminal {
    mid_line: AtomicBool,
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
}

fn summary(name: &str, input: &serde_json::Value) -> String {
    let key = match name {
        "shell" => "command",
        "glob" | "grep" => "pattern",
        _ => "path",
    };
    let s = input[key].as_str().unwrap_or("");
    let first = s.lines().next().unwrap_or("");
    if first.chars().count() > 100 { format!("{}...", first.chars().take(100).collect::<String>()) } else { first.to_string() }
}

impl Ui for Terminal {
    fn event(&self, e: &UiEvent) {
        match e {
            UiEvent::TextDelta(t) => {
                print!("{t}");
                self.mid_line.store(!t.ends_with('\n'), Ordering::Relaxed);
            }
            UiEvent::ReasoningDelta(_) | UiEvent::Usage(_) => return,
            UiEvent::ToolStarted { name, input } => {
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

fn die(msg: &str) -> ! {
    eprintln!("moco: {msg}");
    std::process::exit(2)
}

fn main() {
    let (mut cont, mut prompt, mut dir) = (false, None, None);
    let mut model = std::env::var("MOCO_MODEL").unwrap_or_else(|_| "claude-sonnet-5-5".into());
    let mut agent_id = "build".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = |flag: &str| args.next().unwrap_or_else(|| die(&format!("{flag} needs a value")));
        match a.as_str() {
            "--continue" | "-c" => cont = true,
            "--model" | "-m" => model = value("--model"),
            "--agent" => agent_id = value("--agent"),
            "--dir" => dir = Some(value("--dir")),
            "--prompt" | "-p" => prompt = Some(value("--prompt")),
            "--help" | "-h" => {
                println!("usage: moco [--continue] [--model ID] [--agent build|plan] [--dir PATH] [-p PROMPT]");
                return;
            }
            other => die(&format!("unknown argument {other}")),
        }
    }
    if agent::find(&agent_id).is_none() {
        die(&format!("unknown agent {agent_id}"));
    }

    let app_dir = auth::app_dir();
    if let Some(w) = auth::permissions_warning(&app_dir) {
        eprintln!("warning: {w}");
    }
    let Some(key) = auth::load_key("anthropic", &app_dir) else {
        die(&format!("no API key. Set ANTHROPIC_API_KEY or put {{\"anthropic\": {{\"key\": \"...\"}}}} in {}/auth.json (mode 600)", app_dir.display()));
    };
    let cwd = match dir {
        Some(d) => std::path::PathBuf::from(d).canonicalize().unwrap_or_else(|e| die(&format!("--dir: {e}"))),
        None => std::env::current_dir().unwrap_or_else(|e| die(&e.to_string())),
    };
    let cwd_str = cwd.to_string_lossy().into_owned();
    std::env::set_current_dir(&cwd).ok();

    let store = Store::open(&app_dir.join("data/moco.db")).unwrap_or_else(|e| die(&format!("database: {e}")));
    let recovered = store.recover_claims(3).unwrap_or_default();
    let mut session = match cont.then(|| store.latest_session(&cwd_str).ok().flatten()).flatten() {
        Some(s) => s,
        None => store.create_session(&cwd_str, &agent_id).unwrap_or_else(|e| die(&e.to_string())),
    };
    if cont {
        store.set_agent(&session.id, &agent_id).ok();
    }
    let engine = Engine::new(store, Box::new(Anthropic::new(key, std::env::var("ANTHROPIC_BASE_URL").ok())), EngineConfig::new(&model, app_dir));

    install_sigint();
    let term = Terminal { mid_line: AtomicBool::new(false) };
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
        outcome.ok()
    };

    if recovered.contains(&session.id) {
        println!("{DIM}resuming interrupted run{RESET}");
        run(&session.id, true);
    }
    if let Some(p) = prompt {
        engine.submit(&session.id, &p, Mode::Steer).unwrap_or_else(|e| die(&e.to_string()));
        let ok = matches!(run(&session.id, false), Some(RunOutcome::Idle));
        std::process::exit(if ok { 0 } else { 1 });
    }

    println!("{DIM}moco {} | {} | agent {agent_id} | /help for commands{RESET}", env!("CARGO_PKG_VERSION"), model);
    while let Some(line) = term.read_line("> ") {
        CANCEL.store(false, Ordering::Relaxed);
        let line = line.trim();
        match line {
            "" => continue,
            "/exit" | "/quit" => break,
            "/help" => println!("/agent <build|plan>  switch agent\n/new                 start a new session\n/exit                quit (Ctrl-C interrupts a running turn)"),
            "/new" => match engine.store.lock().unwrap().create_session(&cwd_str, &agent_id) {
                Ok(s) => {
                    session = s;
                    println!("{DIM}new session{RESET}");
                }
                Err(e) => eprintln!("{e}"),
            },
            l if l.starts_with("/agent") => match agent::find(l.trim_start_matches("/agent").trim()) {
                Some(a) => {
                    agent_id = a.id.to_string();
                    engine.store.lock().unwrap().set_agent(&session.id, a.id).ok();
                    println!("{DIM}agent: {}{RESET}", a.name);
                }
                None => println!("unknown agent; available: build, plan"),
            },
            l if l.starts_with('/') => println!("unknown command (try /help)"),
            _ => {
                if engine.submit(&session.id, line, Mode::Steer).is_ok() {
                    run(&session.id, false);
                }
            }
        }
    }
}
