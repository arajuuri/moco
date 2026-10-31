//! Full-screen terminal UI (ratatui + crossterm). One UI thread owns the terminal and an event
//! channel; the engine runs on a worker thread and talks to the UI through `TuiUi`.
mod app;
mod term;

use app::{Action, App, AppEvent, Ev};
use crossterm::event::{self, Event};
use moco_core::engine::{Engine, PermissionPrompt, Reply, Ui, UiEvent};
use moco_core::tools::Question;
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
    pub header: String,
    /// The previous process died mid-run: continue that run first.
    pub recovered: bool,
    /// Expand mentions and queue the prompt; returns notes about attachments.
    pub submit: &'a dyn Fn(&str) -> Result<Vec<String>, String>,
}

/// `UiEvent`s and prompts become `AppEvent`s. Never touches the terminal.
struct TuiUi {
    tx: Mutex<Sender<AppEvent>>,
}

impl TuiUi {
    fn send(&self, e: AppEvent) {
        let _ = self.tx.lock().unwrap().send(e);
    }

    fn convert(&self, depth: usize, agent: &str, e: &UiEvent) {
        let ev = match e {
            UiEvent::Nested { agent, inner } => return self.convert(depth + 1, agent, inner),
            UiEvent::TextDelta(t) => Ev::Text(t.to_string()),
            UiEvent::ReasoningDelta(_) => return,
            UiEvent::ToolStarted { name, input } => Ev::Tool { name: name.to_string(), summary: crate::summary(name, input) },
            UiEvent::ToolFinished { name, ok, text } => Ev::ToolDone { name: name.to_string(), ok: *ok, msg: text.lines().next().unwrap_or("").to_string() },
            UiEvent::Retry { attempt, wait, error } => Ev::Retry { attempt: *attempt, secs: wait.as_secs(), error: error.to_string() },
            UiEvent::StepUsage { usage, cost, session_cost, context_tokens, context_limit } => {
                Ev::Usage { usage: *usage, cost: *cost, session_cost: *session_cost, context: *context_tokens, limit: *context_limit }
            }
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

    fn ask_questions(&self, _questions: &[Question]) -> Option<Vec<Vec<String>>> {
        self.send(AppEvent::Engine { depth: 0, agent: String::new(), ev: Ev::Notice("the assistant asked a question; forms are not available in the full-screen UI yet (use --plain)".into()) });
        None
    }
}

pub fn run(host: Host) -> std::io::Result<()> {
    install_term_handlers();
    let (mut terminal, _guard) = term::enter()?;
    let (tx, rx) = channel::<AppEvent>();
    let cancel = AtomicBool::new(false);
    let stop = AtomicBool::new(false);
    let ui = TuiUi { tx: Mutex::new(tx.clone()) };

    let notify_tx = Mutex::new(tx.clone());
    host.engine.set_notifier(Arc::new(move |line| {
        let _ = notify_tx.lock().unwrap().send(AppEvent::Background(line.to_string()));
    }));

    let mut app = App::new(&host.model, &host.agent);
    app.title = host.title.clone();
    app.push_info(host.header.clone());

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

        let start_run = |app: &mut App, resume: bool| {
            app.running = true;
            cancel.store(false, Ordering::Relaxed);
            let done_tx = tx.clone();
            let (engine, ui, cancel, sid) = (host.engine, &ui, &cancel, host.session_id.as_str());
            scope.spawn(move || {
                let outcome = engine.run(sid, ui, cancel, resume).map_err(|e| e.to_string());
                let _ = done_tx.send(AppEvent::RunDone(outcome));
            });
        };

        if host.recovered {
            app.push_info("resuming interrupted run");
            start_run(&mut app, true);
        }

        let mut quit = false;
        while !quit {
            if SIGNALLED.load(Ordering::Relaxed) {
                break;
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
                match app.update(ev) {
                    Action::None => {}
                    Action::Quit => quit = true,
                    Action::Interrupt => cancel.store(true, Ordering::Relaxed),
                    Action::Poke => {
                        let pending = host.engine.store.lock().unwrap().pending_inbox(&host.session_id).unwrap_or(0);
                        if pending > 0 {
                            start_run(&mut app, false);
                        }
                    }
                    Action::Submit(text) => {
                        if matches!(text.as_str(), "/exit" | "/quit") {
                            quit = true;
                        } else if text == "/help" {
                            app.push_info("Enter sends, Esc or Ctrl-C interrupts, PageUp/PageDown scroll, Ctrl-End follows the output, Ctrl-D quits. Slash commands are not available in the full-screen UI yet: run `moco` (without --tui) for them.");
                        } else if text.starts_with('/') {
                            app.push_info("slash commands are not available in the full-screen UI yet; run `moco` without --tui for them");
                        } else {
                            app.push_user(text.clone());
                            match (host.submit)(&text) {
                                Ok(notes) => {
                                    for n in notes {
                                        app.push_info(n);
                                    }
                                    start_run(&mut app, false);
                                }
                                Err(e) => app.rows.push(app::Row::Error(e)),
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
        while app.running {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(AppEvent::RunDone(_)) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(AppEvent::Ask { reply, .. }) => {
                    let _ = reply.send(Reply::Reject(None));
                }
                _ => {}
            }
        }
        Ok(())
    })
}
