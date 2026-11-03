//! Full-screen terminal UI (ratatui + crossterm). One UI thread owns the terminal and an event
//! channel; the engine runs on a worker thread and talks to the UI through `TuiUi`.
mod app;
mod builtin;
mod complete;
mod diff;
#[cfg(test)]
mod gallery;
mod glyphs;
mod keys;
mod md;
mod term;
mod theme;

pub use complete::Completer;

use app::{Action, App, AppEvent, Ev};
use crossterm::event::{self, Event};
use moco_core::engine::{Engine, PermissionPrompt, Reply, Ui, UiEvent};
use moco_core::store::Mode;
use moco_core::tools::Question;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static SIGNALLED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    SIGNALLED.store(true, Ordering::Relaxed);
}

/// SIGTERM and SIGHUP (a closed SSH connection) end the UI loop so the terminal is restored.
fn install_term_handlers() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_term as extern "C" fn(libc::c_int) as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGHUP, &sa, std::ptr::null_mut());
    }
}

/// Everything the TUI needs from `main`.
pub struct Host<'a> {
    pub engine: &'a Engine,
    pub session_id: String,
    pub model: String,
    pub agent: String,
    pub title: Option<String>,
    /// The previous process died mid-run: continue that run first.
    pub recovered: bool,
    /// Expand mentions and put the prompt in the inbox; returns its inbox id and notes about attachments.
    pub submit: &'a dyn Fn(&str, &str, Mode) -> Result<(String, Vec<String>), String>,
    /// Turn `/name args` into a prompt (custom commands, MCP prompts). `Err` carries a message for the user.
    pub command: &'a dyn Fn(&str) -> Result<Resolved, String>,
    pub completer: Option<Completer>,
    /// Prompt history, one JSON string per line.
    pub history_path: std::path::PathBuf,
    /// Working directory of the session (sessions of other directories cannot be resumed here).
    pub cwd: String,
    /// (id, description) of the skills found.
    pub skills: Vec<(String, String)>,
    /// (name, description) of the custom commands found, subagent ones included.
    pub custom_commands: Vec<(String, String)>,
    pub manager: Option<Arc<moco_core::mcp::Manager>>,
    pub app_dir: std::path::PathBuf,
    pub tui: moco_core::config::TuiConfig,
}

/// A slash command resolved to a prompt. `agent` and `model` apply to this one prompt only.
pub struct Resolved {
    pub text: String,
    pub agent: Option<String>,
    pub model: Option<String>,
}

fn load_history(path: &std::path::Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<String> = text.lines().filter_map(|l| serde_json::from_str::<String>(l).ok()).collect();
    all[all.len().saturating_sub(500)..].to_vec()
}

fn append_history(path: &std::path::Path, entry: &str) {
    use std::io::Write;
    use moco_core::log::LogErr;
    // The history is a convenience: a failure is logged (with the real cause) and never interrupts the session.
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).log_err("tui", "creating the prompt-history directory");
    }
    if let Some(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path).log_err("tui", "opening the prompt history") {
        writeln!(f, "{}", serde_json::Value::String(entry.to_string())).log_err("tui", "writing the prompt history");
    }
}

/// `UiEvent`s and prompts become `AppEvent`s. Never touches the terminal.
struct TuiUi {
    tx: Mutex<Sender<AppEvent>>,
}

impl TuiUi {
    fn send(&self, e: AppEvent) {
        let _ = moco_core::engine::lock(&self.tx).send(e);
    }

    fn convert(&self, depth: usize, agent: &str, e: &UiEvent) {
        let ev = match e {
            UiEvent::Nested { agent, inner } => return self.convert(depth + 1, agent, inner),
            UiEvent::TextDelta(t) => {
                // Fault injection for the pty tests (debug builds only): panic on the engine worker thread when the
                // model's text contains this marker, to check that the UI survives it.
                if cfg!(debug_assertions) && std::env::var("MOCO_TEST_PANIC_ON_TEXT").is_ok_and(|m| !m.is_empty() && t.contains(m.as_str())) {
                    panic!("injected test panic");
                }
                Ev::Text(t.to_string())
            }
            UiEvent::ReasoningDelta(t) => Ev::Reasoning(t.to_string()),
            UiEvent::Delivered => {
                if depth == 0 {
                    self.send(AppEvent::Delivered);
                }
                return;
            }
            UiEvent::ToolStarted { id, name, input } => Ev::Tool { id: id.to_string(), name: name.to_string(), summary: crate::summary(name, input), detail: diff::tool_detail(name, input) },
            UiEvent::ToolFinished { name, ok, text } => Ev::ToolDone { name: name.to_string(), ok: *ok, msg: text.lines().next().unwrap_or("").to_string(), output: diff::output_head(text, 12) },
            UiEvent::Retry { attempt, wait, error } => Ev::Retry { attempt: *attempt, secs: wait.as_secs(), error: error.to_string() },
            UiEvent::StepUsage { usage, cost, session_cost, context_tokens, context_limit } => {
                Ev::Usage { usage: *usage, cost: *cost, session_cost: *session_cost, context: *context_tokens, limit: *context_limit }
            }
            UiEvent::ClassifierUsage { cost, session_cost, classifier_cost } => Ev::ClassifierUsage { cost: *cost, session_cost: *session_cost, classifier_cost: *classifier_cost },
            UiEvent::AutoAllowed { call_id, command, risk, reason } => Ev::AutoAllowed { call_id: call_id.to_string(), command: command.to_string(), risk: risk.to_string(), reason: reason.to_string() },
            UiEvent::Notice(n) => Ev::Notice(n.to_string()),
            UiEvent::Status(s) => Ev::Status(s.to_string()),
            UiEvent::Todos(t) => Ev::Todos(t.to_vec()),
            UiEvent::Titled(t) => Ev::Titled(t.to_string()),
        };
        self.send(AppEvent::Engine { depth, agent: agent.to_string(), ev });
    }
}

impl Ui for TuiUi {
    fn event(&self, e: &UiEvent) {
        self.convert(0, "", e);
    }

    fn ask(&self, p: &PermissionPrompt) -> Reply {
        let (reply, answer) = channel();
        self.send(AppEvent::Ask { action: p.action.to_string(), resources: p.resources.to_vec(), save: p.save.to_vec(), reply });
        // The UI thread answers, or drops the sender when the run is torn down: then reject.
        answer.recv().unwrap_or(Reply::Reject(None))
    }

    fn ask_questions(&self, questions: &[Question]) -> Option<Vec<Vec<String>>> {
        let (reply, answer) = channel();
        self.send(AppEvent::Questions { questions: questions.to_vec(), reply });
        // Answered by the UI thread; dropped (None) when dismissed or the run is torn down.
        answer.recv().ok().flatten()
    }
}

pub fn run(mut host: Host) -> std::io::Result<()> {
    install_term_handlers();
    let (mut terminal, _guard) = term::enter()?;
    let (tx, rx) = channel::<AppEvent>();
    let cancel = AtomicBool::new(false);
    let stop = AtomicBool::new(false);
    let ui = TuiUi { tx: Mutex::new(tx.clone()) };

    let notify_tx = Mutex::new(tx.clone());
    host.engine.set_notifier(Arc::new(move |line| {
        let _ = moco_core::engine::lock(&notify_tx).send(AppEvent::Background(line.to_string()));
    }));

    let mut sid = host.session_id.clone();
    let mut app = App::new(&host.model, &host.agent);
    app.title = host.title.clone();
    app.history = load_history(&host.history_path);
    // Settings from `tui` in the config; mistakes are shown, never fatal.
    let (keymap, key_warnings) = keys::Keymap::from_config(&host.tui.keys, &host.tui.leader);
    app.keymap = keymap;
    let cwd_path = std::path::PathBuf::from(&host.cwd);
    match theme::load(host.tui.theme.as_deref().unwrap_or("auto"), &host.app_dir, &cwd_path, theme::truecolor()) {
        Ok(t) => app.theme = t,
        Err(e) => app.rows.push(app::Row::Error(format!("tui.theme: {e}"))),
    }
    for w in key_warnings {
        app.rows.push(app::Row::Error(w));
    }
    app.sidebar = match &host.tui.sidebar {
        Some(serde_json::Value::Bool(b)) => Some(*b),
        _ => None,
    };
    if let Some(n) = host.tui.away_after {
        app.away_after = Duration::from_secs(n);
    }
    app.glyphs = glyphs::choose(host.tui.ascii.unwrap_or(false));
    let bell = host.tui.bell.unwrap_or(true);
    if let Some(completer) = host.completer.take() {
        app.completer = Some(Box::new(move |t| completer.items(t)));
    }
    // Agent and model that a slash command overrode for one prompt, restored when its run ends.
    let restore: Mutex<Option<(String, Option<String>)>> = Mutex::new(None);
    let engine = host.engine;

    // Show the stored conversation of the session we start in (--continue, --session).
    let load_rows = |app: &mut App, sid: &str| load_session_view(&engine.store(), app, sid);
    // Model, agent and title as stored for the session.
    let refresh_status = |app: &mut App, sid: &str| {
        if let Some(s) = engine.store().session(sid).ok().flatten() {
            app.model = engine.model_ref_for(&s);
            app.agent = s.agent.clone();
            app.title = s.title.clone();
            let info = engine.effort_info(&s);
            app.effort = info.desired;
            app.effort_levels = info.levels;
            app.permission_mode = engine.permission_mode_of(&s);
        }
        app.cost_limits = engine.limits();
    };
    load_rows(&mut app, &sid);
    refresh_status(&mut app, &sid);
    app.rows.insert(0, app::Row::Banner { title: format!("moco {}", env!("CARGO_PKG_VERSION")), model: host.model.clone(), agent: host.agent.clone(), cwd: crate::tilde(&host.cwd) });

    std::thread::scope(|scope| -> std::io::Result<()> {
        // Terminal events. Polls with a timeout so it can stop when the UI loop ends.
        let key_tx = tx.clone();
        let stop = &stop;
        scope.spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match event::poll(Duration::from_millis(100)) {
                    Ok(true) => {
                        let ev = match event::read() {
                            Ok(Event::Key(k)) => AppEvent::Key(k),
                            Ok(Event::Paste(s)) => AppEvent::Paste(s),
                            Ok(Event::Resize(..)) => AppEvent::Resize,
                            Ok(Event::FocusGained) => AppEvent::Focus(true),
                            Ok(Event::FocusLost) => AppEvent::Focus(false),
                            Ok(_) => continue,
                            Err(_) => break,
                        };
                        if key_tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        });
        // Stop the input thread however this closure ends: on an early `?` return the scope would otherwise wait
        // for it forever and moco would hang instead of exiting with the error.
        struct StopInput<'a>(&'a AtomicBool);
        impl Drop for StopInput<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let _stop_input = StopInput(stop);

        let start_run = |app: &mut App, sid: &str, resume: bool| {
            app.running = true;
            cancel.store(false, Ordering::Relaxed);
            let done_tx = tx.clone();
            let (ui, cancel, sid) = (&ui, &cancel, sid.to_string());
            scope.spawn(move || {
                // A panic in the engine must still end the run in the UI, or the turn would look endless.
                let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.run(&sid, ui, cancel, resume))) {
                    Ok(r) => r.map_err(|e| format!("store error: {e}")),
                    Err(p) => Err(format!("the engine crashed: {}", panic_message(p.as_ref()))),
                };
                let _ = done_tx.send(AppEvent::RunDone(outcome));
            });
        };

        // Queue a prompt for the session and start the run, showing attachment notes.
        let send_prompt = |app: &mut App, sid: &str, text: &str| match (host.submit)(sid, text, Mode::Steer) {
            Ok((_, notes)) => {
                for n in notes {
                    app.push_info(n);
                }
                start_run(app, sid, false);
            }
            Err(e) => app.rows.push(app::Row::Error(e)),
        };

        if host.recovered {
            app.push_info("resuming interrupted run");
            start_run(&mut app, &sid, true);
        }

        // Prompts that were sent during a turn leave the inbox when the engine delivers them: that is when
        // they become part of the conversation, so that is when they appear in the transcript.
        let sync_queue = |app: &mut App, sid: &str| {
            let waiting = engine.store().inbox_items(sid).unwrap_or_default();
            let (still, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut app.queued).into_iter().partition(|q| waiting.iter().any(|w| w.id == q.id));
            for q in gone {
                app.push_user(q.text);
            }
            app.queued = still
                .into_iter()
                .map(|mut q| {
                    q.steer = waiting.iter().find(|w| w.id == q.id).is_none_or(|w| w.steer);
                    q
                })
                .collect();
            app.jobs = engine.running_jobs();
            if app.sidebar_shown() {
                app.mcp = host.manager.as_ref().map(|m| m.status().iter().map(|i| (i.name.clone(), moco_core::mcp::manager::describe(i))).collect()).unwrap_or_default();
                app.job_list = engine.background_jobs(None).iter().filter(|j| j.status.starts_with("running")).map(|j| (j.id.clone(), format!("{} {}s", j.kind, j.seconds))).collect();
            }
        };
        let mut last_sync = std::time::Instant::now();
        let mut title_marked = false;
        let mut actions: VecDeque<Action> = VecDeque::new();
        let mut quit = false;
        while !quit {
            if SIGNALLED.load(Ordering::Relaxed) {
                break;
            }
            if (app.running || !app.queued.is_empty() || app.jobs > 0 || app.sidebar_shown()) && last_sync.elapsed() >= Duration::from_millis(200) {
                sync_queue(&mut app, &sid);
                last_sync = std::time::Instant::now();
            }
            terminal.draw(|f| app::render(f, &mut app))?;
            let first = match rx.recv_timeout(Duration::from_millis(150)) {
                Ok(e) => e,
                Err(RecvTimeoutError::Timeout) => AppEvent::Tick,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            // Handle everything that is already waiting before drawing again (streamed deltas pile up).
            let mut next = Some(first);
            let mut handled = 0;
            while let Some(ev) = next.take() {
                if matches!(ev, AppEvent::RunDone(_)) {
                    if term::reenter_after_panic(&mut terminal)? {
                        last_sync = std::time::Instant::now();
                    }
                    if let Some((agent, model)) = moco_core::engine::lock(&restore).take() {
                        let st = engine.store();
                        let (a, m) = (st.set_agent(&sid, &agent), st.set_model(&sid, model.as_deref()));
                        builtin::stored_ok(&mut app, "the agent and model of this session (they stay switched after the custom command)", a.and(m));
                    }
                }
                if matches!(ev, AppEvent::Delivered) {
                    // The engine moved waiting prompts into the conversation: show them before what follows.
                    sync_queue(&mut app, &sid);
                    last_sync = std::time::Instant::now();
                }
                let finished_idle = matches!(ev, AppEvent::RunDone(Ok(moco_core::engine::RunOutcome::Idle)));
                let was_key = matches!(ev, AppEvent::Key(_) | AppEvent::Focus(true));
                actions.push_back(app.update(ev));
                if was_key && title_marked {
                    title_marked = false;
                    let _ = crossterm::execute!(terminal.backend_mut(), crossterm::terminal::SetTitle("moco"));
                }
                if let Some(msg) = app.attention.take() {
                    if bell && app.away() {
                        // A bell and a marked window title: the terminal can flag the window or tab.
                        let clean: String = msg.chars().filter(|c| !c.is_control()).take(80).collect();
                        let out = terminal.backend_mut();
                        let _ = std::io::Write::write_all(out, b"\x07");
                        let _ = crossterm::execute!(out, crossterm::terminal::SetTitle(format!("moco: {clean}")));
                        title_marked = true;
                    }
                }
                if finished_idle {
                    // A prompt that arrived as the turn ended is still waiting: run again to deliver it.
                    sync_queue(&mut app, &sid);
                    if !app.queued.is_empty() && !app.running {
                        start_run(&mut app, &sid, false);
                    }
                }
                while let Some(action) = actions.pop_front() {
                    match action {
                        Action::None => {}
                        Action::Quit => quit = true,
                        Action::Interrupt => cancel.store(true, Ordering::Relaxed),
                        Action::Poke => {
                            let pending = engine.store().pending_inbox(&sid).unwrap_or(0);
                            if pending > 0 {
                                start_run(&mut app, &sid, false);
                            }
                        }
                        Action::Enqueue { text, shown, queue } => match (host.submit)(&sid, &text, if queue { Mode::Queue } else { Mode::Steer }) {
                            Ok((id, notes)) => {
                                for n in notes {
                                    app.push_info(n);
                                }
                                app.queued.push(app::Queued { id, steer: !queue, text: shown });
                                if !app.running {
                                    sync_queue(&mut app, &sid);
                                    start_run(&mut app, &sid, false); // the turn ended meanwhile
                                }
                            }
                            Err(e) => app.rows.push(app::Row::Error(e)),
                        },
                        Action::Dropped { kind, value } => match kind {
                            app::PickKind::Queue => {
                                let cancelled = engine.store().cancel_inbox(&value);
                                // On failure the prompt is still waiting and will run: keep showing it.
                                if builtin::stored_ok(&mut app, "the removal of the waiting prompt", cancelled) {
                                    app.queued.retain(|q| q.id != value);
                                    app.push_info("removed the waiting prompt");
                                }
                            }
                            app::PickKind::Job => match engine.kill_job(&value) {
                                Ok(t) => app.push_info(t),
                                Err(e) => app.rows.push(app::Row::Error(e)),
                            },
                            _ => {}
                        },
                        Action::SetPermissionMode(mode) => builtin::set_permission_mode(engine, &sid, &mut app, mode),
                        Action::SetEffort(level) => {
                            builtin::choose_effort(engine, &sid, &mut app, level.as_deref());
                            refresh_status(&mut app, &sid);
                        }
                        Action::Palette => app.open_picker(app::Picker::new(app::PickKind::Command, "commands", builtin::palette_items(&host), "")),
                        Action::Picked { kind, value } => match kind {
                            app::PickKind::Model => {
                                builtin::choose_model(engine, &sid, &mut app, &value, "");
                                refresh_status(&mut app, &sid);
                            }
                            app::PickKind::Effort => {
                                builtin::choose_effort(engine, &sid, &mut app, (value != "default").then_some(value.as_str()));
                                refresh_status(&mut app, &sid);
                            }
                            app::PickKind::Agent => {
                                builtin::choose_agent(engine, &sid, &mut app, &value, &value);
                                refresh_status(&mut app, &sid);
                            }
                            app::PickKind::Session => {
                                let found = engine.store().session(&value).ok().flatten();
                                if let Some(s) = found {
                                    if app.running {
                                        app.rows.push(app::Row::Error("wait for the running turn to finish (or press Esc) before switching sessions".into()));
                                    } else if let builtin::Effect::Switch(s) = builtin::resume(&host, &mut app, s) {
                                        sid = s.id.clone();
                                        load_rows(&mut app, &sid);
                                        refresh_status(&mut app, &sid);
                                        app.push_info(format!("session {}: {}", crate::short(&sid), s.title.unwrap_or_else(|| "(untitled)".into())));
                                    }
                                }
                            }
                            app::PickKind::Queue => {
                                if let Some(q) = app.queued.iter().find(|q| q.id == value).cloned() {
                                    let mode = if q.steer { Mode::Queue } else { Mode::Steer };
                                    if engine.store().set_inbox_mode(&value, mode).unwrap_or(false) {
                                        sync_queue(&mut app, &sid);
                                        app.push_info(format!("\"{}\" now goes {}", crate::clip(q.text.lines().next().unwrap_or(""), 40), if q.steer { "after the turn" } else { "into the next step" }));
                                    } else {
                                        app.push_info("it was delivered already");
                                    }
                                }
                            }
                            app::PickKind::Theme => builtin::set_theme(&host, &mut app, &value),
                            app::PickKind::Job => match engine.job_output(&value, 40) {
                                Ok(t) => app.rows.push(app::Row::Shell(t)),
                                Err(e) => app.rows.push(app::Row::Error(e)),
                            },
                            app::PickKind::Command => {
                                if let Some(cmd) = value.strip_prefix("run:") {
                                    actions.push_back(Action::Submit { text: cmd.to_string(), shown: cmd.to_string() });
                                } else if let Some(cmd) = value.strip_prefix("ins:") {
                                    app.input = cmd.to_string();
                                    app.cursor = cmd.chars().count();
                                    app.refresh_popup();
                                }
                            }
                        },
                        Action::Submit { text, shown } => {
                            append_history(&host.history_path, &shown);
                            let (name, args) = text.strip_prefix('/').map(|r| r.split_once(char::is_whitespace).unwrap_or((r, ""))).unwrap_or(("", ""));
                            if matches!(text.as_str(), "/exit" | "/quit") {
                                quit = true;
                            } else if text == "/help" {
                                {
                                    use keys::KeyAction as K;
                                    let km = &app.keymap;
                                    let help = format!(
                                        "Enter sends, {} adds a line, Up/Down move or browse history, Tab completes / and @ (or switches the permission mode when no completion is open), {} opens the command palette, {} interrupts (Ctrl-C too), {}/{} scroll, {} follows the output, {} cycles the detail level, {} quits. Leader {}: {}. !command runs a shell command. Commands: /model /agent /mode /sessions /resume /new /rename /fork /archive /undo /redo /diff /compact /queue /jobs /theme /skills /commands /mcp /exit.",
                                        km.label(K::Newline),
                                        km.label(K::Palette),
                                        km.label(K::Interrupt),
                                        km.label(K::PageUp),
                                        km.label(K::PageDown),
                                        km.label(K::Bottom),
                                        km.label(K::Detail),
                                        km.label(K::Quit),
                                        km.label(K::Leader),
                                        km.leader_hint()
                                    );
                                    app.push_info(help);
                                }
                            } else if let Some(cmd) = text.strip_prefix('!') {
                                let cmd = cmd.trim().to_string();
                                app.push_user(shown.clone());
                                if cmd.is_empty() {
                                    app.push_info("usage: !<shell command>");
                                } else {
                                    app.running = true;
                                    cancel.store(false, Ordering::Relaxed);
                                    let done_tx = tx.clone();
                                    let (cancel, sid) = (&cancel, sid.clone());
                                    scope.spawn(move || {
                                        let r = engine.run_user_command(&sid, &cmd, cancel);
                                        let _ = done_tx.send(AppEvent::ShellDone(r));
                                    });
                                }
                            } else if text.starts_with('/') {
                                app.push_user(shown.clone());
                                let is_builtin = builtin::NAMES.iter().any(|(n, _)| *n == name) || matches!(name, "resume" | "unarchive");
                                if is_builtin {
                                    if app.running && !matches!(name, "jobs" | "skills" | "commands" | "mcp" | "diff" | "queue" | "theme") {
                                        app.rows.push(app::Row::Error("a turn is running: wait for it (or press Esc) first".into()));
                                    } else {
                                        let agent = app.agent.clone();
                                        match builtin::run(&host, &mut app, &sid, &agent, name, args) {
                                            Some(builtin::Effect::Switch(s)) => {
                                                sid = s.id.clone();
                                                load_rows(&mut app, &sid);
                                                refresh_status(&mut app, &sid);
                                                app.push_info(format!("session {}: {}", crate::short(&sid), s.title.unwrap_or_else(|| "(untitled)".into())));
                                            }
                                            Some(builtin::Effect::Reload) => {
                                                let keep: Vec<app::Row> = std::mem::take(&mut app.rows).into_iter().rev().take_while(|r| !matches!(r, app::Row::User(_))).collect();
                                                load_rows(&mut app, &sid);
                                                app.rows.extend(keep.into_iter().rev());
                                            }
                                            Some(builtin::Effect::Enqueued) => {
                                                if !app.running {
                                                    sync_queue(&mut app, &sid);
                                                    start_run(&mut app, &sid, false);
                                                }
                                            }
                                            Some(builtin::Effect::Compact) => {
                                                app.running = true;
                                                cancel.store(false, Ordering::Relaxed);
                                                let (ui, cancel, done_tx, sid) = (&ui, &cancel, tx.clone(), sid.clone());
                                                scope.spawn(move || {
                                                    use moco_core::engine::CompactResult::*;
                                                    let r = match engine.compact(&sid, ui, cancel) {
                                                        Ok(Compacted { .. }) => Ok("compacted".to_string()),
                                                        Ok(NothingToCompact) => Ok("nothing to compact yet (recent messages are kept as they are)".into()),
                                                        Ok(Interrupted) => Ok("interrupted".into()),
                                                        Ok(Failed(e)) => Err(format!("compaction failed: {e}")),
                                                        Err(e) => Err(format!("store error: {e}")),
                                                    };
                                                    let _ = done_tx.send(AppEvent::OpDone(r));
                                                });
                                            }
                                            _ => {}
                                        }
                                        refresh_status(&mut app, &sid);
                                    }
                                } else {
                                    match (host.command)(&text) {
                                        Ok(r) => {
                                            if r.agent.is_some() || r.model.is_some() {
                                                let st = engine.store();
                                                if let Some(cur) = st.session(&sid).ok().flatten() {
                                                    *moco_core::engine::lock(&restore) = Some((cur.agent.clone(), cur.model.clone()));
                                                }
                                                if let Some(a) = r.agent.as_ref().filter(|a| engine.agents().iter().any(|x| x.id == **a)) {
                                                    let stored = st.set_agent(&sid, a);
                                                    builtin::stored_ok(&mut app, "the command's agent", stored);
                                                }
                                                if let Some(m) = &r.model {
                                                    let stored = st.set_model(&sid, Some(m));
                                                    builtin::stored_ok(&mut app, "the command's model", stored);
                                                }
                                            }
                                            send_prompt(&mut app, &sid, &r.text);
                                        }
                                        Err(e) => app.rows.push(app::Row::Error(e)),
                                    }
                                }
                            } else {
                                app.push_user(shown.clone());
                                send_prompt(&mut app, &sid, &text);
                            }
                        }
                    }
                }
                handled += 1;
                if handled < 256 {
                    next = rx.try_recv().ok();
                }
            }
        }

        // Leaving: stop a running turn and wait for it, so nothing keeps writing into a restored terminal.
        cancel.store(true, Ordering::Relaxed);
        stop.store(true, Ordering::Relaxed);
        app.dialogs.clear(); // dropping the senders rejects whatever the run was waiting on
        app.forms.clear();
        if app.running {
            loop {
                match rx.recv_timeout(Duration::from_secs(1)) {
                    Ok(AppEvent::RunDone(_)) | Ok(AppEvent::ShellDone(_)) | Ok(AppEvent::OpDone(_)) | Err(RecvTimeoutError::Disconnected) => break,
                    Ok(AppEvent::Ask { reply, .. }) => {
                        let _ = reply.send(Reply::Reject(None));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    })
}

/// Replaces what the UI shows (transcript, costs, sidebar todo list) with the stored state of session `sid`. A session
/// without a stored list clears the previous session's list.
fn load_session_view(st: &moco_core::store::Store, app: &mut App, sid: &str) {
    let messages = st.messages(sid).unwrap_or_default();
    let revert = st.revert(sid).ok().flatten();
    let hidden = revert.as_ref().and_then(|r| r["messageId"].as_str().map(String::from));
    app.rows = builtin::history_rows(&messages, hidden.as_deref());
    app.session_cost = st.usage_totals(sid).map(|t| t.0).unwrap_or(0.0);
    app.classifier_cost = st.classifier_cost(sid).unwrap_or(0.0);
    app.todos = st.todos(sid).unwrap_or_default();
    app.scroll = 0;
}

/// The text of a caught panic payload.
fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown panic".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use moco_core::tools::Todo;

    fn todo(content: &str, status: &str) -> Todo {
        Todo { content: content.into(), status: status.into() }
    }

    #[test]
    fn resuming_a_session_restores_its_todo_list_and_switching_clears_it() {
        let st = moco_core::store::Store::open_memory().unwrap();
        let with = st.create_session("/tmp/p", "build").unwrap();
        let without = st.create_session("/tmp/p", "build").unwrap();
        st.set_todos(&with.id, &[todo("write tests", "in_progress"), todo("ship", "pending")]).unwrap();

        let mut app = App::new("m", "build");
        load_session_view(&st, &mut app, &with.id);
        assert_eq!(app.todos, vec![todo("write tests", "in_progress"), todo("ship", "pending")]);

        // Switching to a session that has no list must not keep the previous one.
        load_session_view(&st, &mut app, &without.id);
        assert!(app.todos.is_empty());
    }

    #[test]
    fn history_round_trips_multiline_entries_and_keeps_the_last_500() {
        let dir = std::env::temp_dir().join(format!("moco-hist-{}", std::process::id()));
        let path = dir.join("data/tui-history");
        assert!(load_history(&path).is_empty());
        append_history(&path, "one\ntwo \"quoted\"");
        append_history(&path, "second");
        assert_eq!(load_history(&path), vec!["one\ntwo \"quoted\"", "second"]);
        for i in 0..600 {
            append_history(&path, &format!("e{i}"));
        }
        let h = load_history(&path);
        assert_eq!((h.len(), h[0].as_str(), h[499].as_str()), (500, "e100", "e599"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
