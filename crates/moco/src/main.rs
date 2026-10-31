//! moco: line-based terminal chat.
use moco_core::catalog::{Catalog, ProviderInfo};
use moco_core::engine::{Engine, EngineConfig, PermissionPrompt, Reply, RunOutcome, Ui, UiEvent};
use moco_core::provider::registry;
use moco_core::provider::Usage;
use moco_core::store::{Mode, Store};
use moco_core::{auth, config};
use std::io::{BufRead, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

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

#[derive(Default)]
struct RunUsage {
    usage: Usage,
    cost: f64,
    priced: bool,
    session_cost: f64,
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
        let cost = if r.priced { format!("${:.4} (session ${:.4})", r.cost, r.session_cost) } else { "cost n/a".to_string() };
        let ctx = match r.limit {
            Some(l) if l > 0 => format!("context {}% of {}", (r.context * 100 / l).min(100), human(l)),
            _ => format!("context {}", human(r.context)),
        };
        println!("{DIM}{} in, {} out, {} cached | {cost} | {ctx}{RESET}", human(r.usage.input), human(r.usage.output), human(r.usage.cache_read));
    }
}

fn human(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{:.1}k", n as f64 / 1000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
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
            UiEvent::ReasoningDelta(_) => return,
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
    format!("{:<16} {:>4} models  {state}", p.id, p.models.len())
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

    let (mut cont, mut prompt, mut dir) = (false, None, None);
    let mut model = std::env::var("MOCO_MODEL").ok();
    let mut agent_id: Option<String> = None;
    let mut args = raw.drain(..);
    while let Some(a) = args.next() {
        let mut value = |flag: &str| args.next().unwrap_or_else(|| die(&format!("{flag} needs a value")));
        match a.as_str() {
            "--continue" | "-c" => cont = true,
            "--model" | "-m" => model = Some(value("--model")),
            "--agent" => agent_id = Some(value("--agent")),
            "--dir" => dir = Some(value("--dir")),
            "--prompt" | "-p" => prompt = Some(value("--prompt")),
            "--help" | "-h" => {
                println!(
                    "usage: moco [--continue] [--model PROVIDER/MODEL] [--agent ID] [--dir PATH] [-p PROMPT]\n       moco models [FILTER] [--refresh]\n       moco auth PROVIDER"
                );
                return;
            }
            other => die(&format!("unknown argument {other}")),
        }
    }
    if let Some(w) = auth::permissions_warning(&app_dir) {
        eprintln!("warning: {w}");
    }
    let cwd = match dir {
        Some(d) => std::path::PathBuf::from(d).canonicalize().unwrap_or_else(|e| die(&format!("--dir: {e}"))),
        None => std::env::current_dir().unwrap_or_else(|e| die(&e.to_string())),
    };
    let cwd_str = cwd.to_string_lossy().into_owned();
    std::env::set_current_dir(&cwd).ok();

    let loaded = config::load(&app_dir, &cwd);
    for w in &loaded.warnings {
        eprintln!("warning: config: {w}");
    }
    let cfg = loaded.config;
    if let (Some(sh), None) = (&cfg.shell, std::env::var_os("MOCO_SHELL")) {
        std::env::set_var("MOCO_SHELL", sh);
    }
    let agents = config::resolve_agents(&cfg);
    let mut agent_id = agent_id.or(cfg.default_agent.clone()).unwrap_or_else(|| "build".into());
    if !agents.iter().any(|a| a.id == agent_id) {
        die(&format!("unknown agent {agent_id}; available: {}", agents.iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join(", ")));
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

    let store = Store::open(&app_dir.join("data/moco.db")).unwrap_or_else(|e| die(&format!("database: {e}")));
    let recovered = store.recover_claims(3).unwrap_or_default();
    let mut session = match cont.then(|| store.latest_session(&cwd_str).ok().flatten()).flatten() {
        Some(s) => s,
        None => store.create_session(&cwd_str, &agent_id).unwrap_or_else(|e| die(&e.to_string())),
    };
    if cont {
        store.set_agent(&session.id, &agent_id).ok();
    }
    // An explicit --model / MOCO_MODEL applies to this session; otherwise a resumed session keeps its own.
    let explicit_model = std::env::args().any(|a| a == "--model" || a == "-m") || std::env::var("MOCO_MODEL").is_ok();
    if explicit_model {
        store.set_model(&session.id, Some(&model)).ok();
        session.model = Some(model.clone());
    }

    let mut engine_cfg = EngineConfig::new(&model, app_dir.clone());
    engine_cfg.agents = agents;
    engine_cfg.instructions = cfg.instructions.clone();
    engine_cfg.catalog = catalog;
    let n_sources = loaded.sources.len();
    let engine = Engine::with_providers(store, providers, engine_cfg);

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

    if recovered.contains(&session.id) {
        println!("{DIM}resuming interrupted run{RESET}");
        run(&session.id, true);
    }
    if let Some(p) = prompt {
        engine.submit(&session.id, &p, Mode::Steer).unwrap_or_else(|e| die(&e.to_string()));
        let ok = matches!(run(&session.id, false), Some(RunOutcome::Idle));
        std::process::exit(if ok { 0 } else { 1 });
    }

    let current = |sid: &str| engine.store.lock().unwrap().session(sid).ok().flatten().map(|s| engine.model_ref_for(&s)).unwrap_or_default();
    println!("{DIM}moco {} | {} | agent {agent_id} | {n_sources} config file(s) | /help for commands{RESET}", env!("CARGO_PKG_VERSION"), current(&session.id));
    while let Some(line) = term.read_line("> ") {
        CANCEL.store(false, Ordering::Relaxed);
        let line = line.trim();
        match line {
            "" => continue,
            "/exit" | "/quit" => break,
            "/help" => println!(
                "/model [FILTER|PROVIDER/MODEL]  show, search or switch the model\n/agent <id>                     switch agent\n/new                            start a new session\n/exit                           quit (Ctrl-C interrupts a running turn)"
            ),
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
            l if l.starts_with("/agent") => {
                let want = l.trim_start_matches("/agent").trim();
                match engine.agents().iter().find(|a| a.id == want) {
                    Some(a) => {
                        agent_id = a.id.clone();
                        engine.store.lock().unwrap().set_agent(&session.id, &a.id).ok();
                        println!("{DIM}agent: {}{RESET}", a.name);
                    }
                    None => println!("unknown agent; available: {}", engine.agents().iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join(", ")),
                }
            }
            l if l.starts_with('/') => println!("unknown command (try /help)"),
            _ => {
                if engine.submit(&session.id, line, Mode::Steer).is_ok() {
                    run(&session.id, false);
                }
            }
        }
    }
}
