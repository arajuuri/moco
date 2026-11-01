//! Background jobs: shell commands and subagents that keep running after the tool call that started them returned.
//! When one ends, its result is queued as a synthetic message in the owning session (delivered at the next step
//! boundary of a running turn, or with the next prompt), and the UI is told through the notifier.
use super::*;
use crate::tools::{shell, JobInfo};
use std::path::Path;
use std::time::Instant;

#[derive(Debug, Clone, PartialEq)]
enum State {
    Running,
    Exited { code: Option<i32>, signal: Option<i32> },
    TimedOut,
    Stopped,
    Finished,
    Failed(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Shell,
    Subagent,
}

struct Job {
    id: String,
    kind: Kind,
    session_id: String,
    title: String,
    started: Instant,
    state: State,
    out: Option<PathBuf>,
    /// Setting this stops the job (kills the process group, or cancels the subagent's run).
    stop: Arc<AtomicBool>,
}

#[derive(Default)]
pub(super) struct Jobs {
    list: Mutex<Vec<Job>>,
}

fn describe(state: &State) -> String {
    match state {
        State::Running => "running".into(),
        State::Exited { code: Some(c), .. } => format!("exited (code {c})"),
        State::Exited { signal: Some(s), .. } => format!("killed by signal {s}"),
        State::Exited { .. } => "exited".into(),
        State::TimedOut => "timed out".into(),
        State::Stopped => "stopped".into(),
        State::Finished => "finished".into(),
        State::Failed(e) => format!("failed: {e}"),
    }
}

/// Answers to permission questions from a background subagent: nobody is at the keyboard for it.
struct Unattended;

impl Ui for Unattended {
    fn event(&self, _: &UiEvent) {}
    fn ask(&self, _: &PermissionPrompt) -> Reply {
        Reply::Reject(Some("This needs approval, but you are a subagent running in the background and nobody is present to give it. Do not retry it; do what is possible without it and say what needs approval.".into()))
    }
}

impl Engine {
    /// Put the engine behind an `Arc` and give it a handle to itself, which background jobs need.
    pub fn share(self) -> Arc<Engine> {
        let a = Arc::new(self);
        *a.me.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(&a);
        a
    }

    pub(super) fn shared(&self) -> Result<Arc<Engine>, ToolError> {
        self.me.lock().unwrap_or_else(|e| e.into_inner()).upgrade().ok_or_else(|| ToolError::fail("background jobs are not available in this setup"))
    }

    /// Called (from job threads) when a background job ends; the UI shows the line.
    pub fn set_notifier(&self, f: Arc<dyn Fn(&str) + Send + Sync>) {
        *self.notifier.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    fn notify(&self, line: &str) {
        if let Some(f) = self.notifier.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            f(line);
        }
    }

    fn jobs_lock(&self) -> std::sync::MutexGuard<'_, Vec<Job>> {
        self.jobs.list.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn register(&self, job: Job) {
        let mut list = self.jobs_lock();
        list.push(job);
        // Keep the list short: forget the oldest finished jobs.
        while list.len() > 60 {
            match list.iter().position(|j| j.state != State::Running) {
                Some(i) => {
                    list.remove(i);
                }
                None => break,
            }
        }
    }

    fn set_state(&self, id: &str, state: State) {
        if let Some(j) = self.jobs_lock().iter_mut().find(|j| j.id == id) {
            j.state = state;
        }
    }

    /// Queue `text` for the session as a message the model will see.
    fn tell_session(&self, sid: &str, text: &str, description: &str) {
        let _ = self.store().enqueue(sid, None, Mode::Steer, "synthetic", &json!({"text": text, "description": description}));
    }

    pub fn background_jobs(&self, session: Option<&str>) -> Vec<JobInfo> {
        self.jobs_lock()
            .iter()
            .filter(|j| session.is_none_or(|s| j.session_id == s))
            .map(|j| JobInfo {
                id: j.id.clone(),
                kind: if j.kind == Kind::Shell { "shell" } else { "subagent" }.into(),
                title: j.title.clone(),
                status: describe(&j.state),
                running: j.state == State::Running,
                seconds: j.started.elapsed().as_secs(),
                output_path: j.out.clone(),
            })
            .collect()
    }

    pub fn running_jobs(&self) -> usize {
        self.jobs_lock().iter().filter(|j| j.state == State::Running).count()
    }

    /// Wait until no job is running (true), or `max` passes or `cancel` is set (false).
    pub fn wait_for_jobs(&self, cancel: &AtomicBool, max: Duration) -> bool {
        let end = Instant::now() + max;
        while self.running_jobs() > 0 {
            if cancel.load(Ordering::Relaxed) || Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        true
    }

    /// Stop everything still running (on exit). Returns how many jobs were running.
    pub fn kill_all_jobs(&self) -> usize {
        let running: Vec<Arc<AtomicBool>> = self.jobs_lock().iter().filter(|j| j.state == State::Running).map(|j| j.stop.clone()).collect();
        for s in &running {
            s.store(true, Ordering::Relaxed);
        }
        let end = Instant::now() + Duration::from_secs(3);
        while self.running_jobs() > 0 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        running.len()
    }

    /// Ask a job to stop and wait briefly for it to end.
    pub fn kill_job(&self, id: &str) -> Result<String, String> {
        let (stop, running) = {
            let list = self.jobs_lock();
            let j = list.iter().find(|j| j.id == id).ok_or_else(|| format!("no background job '{id}'"))?;
            (j.stop.clone(), j.state == State::Running)
        };
        if !running {
            return Ok(format!("{id} is not running ({})", self.background_jobs(None).iter().find(|j| j.id == id).map(|j| j.status.clone()).unwrap_or_default()));
        }
        stop.store(true, Ordering::Relaxed);
        let end = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end && self.jobs_lock().iter().any(|j| j.id == id && j.state == State::Running) {
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(format!("Stopped {id}."))
    }

    /// Output so far of a job, with its state.
    pub fn job_output(&self, id: &str, lines: usize) -> Result<String, String> {
        let (kind, out, state, sid) = {
            let list = self.jobs_lock();
            let j = list.iter().find(|j| j.id == id).ok_or_else(|| format!("no background job '{id}'"))?;
            (j.kind, j.out.clone(), j.state.clone(), j.session_id.clone())
        };
        let head = format!("[{id}: {}]", describe(&state));
        match (kind, out) {
            (Kind::Shell, Some(path)) => {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let all: Vec<&str> = text.lines().collect();
                let tail = all[all.len().saturating_sub(lines)..].join("\n");
                Ok(if tail.is_empty() { format!("{head}\n(no output yet)") } else { format!("{head}\n{tail}") })
            }
            _ => {
                let _ = sid;
                Ok(match self.last_assistant_text(id) {
                    Some(t) => format!("{head}\n{t}"),
                    None => format!("{head}\n(the subagent has not produced text yet)"),
                })
            }
        }
    }

    /// Run `command` in the background. The job survives the tool call; its end is reported to `session`.
    pub(super) fn start_shell_job(self: &Arc<Self>, session: &Session, command: &str, workdir: &Path, timeout: Option<Duration>, unsandboxed: bool) -> Result<(String, PathBuf), ToolError> {
        let id = crate::id::new_id("shl");
        let out = self.spill_dir().join(format!("{id}.txt"));
        // Background commands inherit the sandbox like foreground ones.
        let root = (!unsandboxed).then(|| crate::tools::project_root_of(Path::new(&session.directory)));
        let sandboxed = root.is_some() && crate::sandbox::active();
        let mut child = shell::spawn(command, workdir, &out, root.as_deref())?;
        let stop = Arc::new(AtomicBool::new(false));
        self.register(Job { id: id.clone(), kind: Kind::Shell, session_id: session.id.clone(), title: command.to_string(), started: Instant::now(), state: State::Running, out: Some(out.clone()), stop: stop.clone() });
        let (eng, sid, cmd, job_id, out_path) = (self.clone(), session.id.clone(), command.to_string(), id.clone(), out.clone());
        std::thread::spawn(move || {
            let mut f = shell::wait(&mut child, &out_path, timeout, &|| stop.load(Ordering::Relaxed), true);
            if sandboxed {
                crate::sandbox::annotate(&mut f.text, f.exit);
            }
            eng.finish_shell_job(&job_id, &sid, &cmd, &f);
        });
        Ok((id, out))
    }

    fn finish_shell_job(&self, id: &str, sid: &str, command: &str, f: &shell::Finished) {
        let state = if f.killed {
            State::Stopped
        } else if f.timed_out {
            State::TimedOut
        } else {
            State::Exited { code: f.exit, signal: f.signal }
        };
        let what = describe(&state);
        self.set_state(id, state.clone());
        let output = if f.text.trim().is_empty() { "(no output)".to_string() } else { f.text.clone() };
        // Whoever stopped it knows; everything else is news for the model.
        if state != State::Stopped {
            self.tell_session(
                sid,
                &format!("Background command finished (shell ID: {id}): {what}.\nCommand: {command}\n\n{output}"),
                &format!("background shell {id} {what}"),
            );
        }
        self.notify(&format!("background {id} `{}` {what}", command.lines().next().unwrap_or("").chars().take(60).collect::<String>()));
    }

    /// Run the prepared child session on a thread of its own, unattended; report its result to `parent`.
    pub(super) fn start_subagent_job(self: &Arc<Self>, parent: &Session, child: &Session, description: &str) {
        let stop = Arc::new(AtomicBool::new(false));
        self.register(Job { id: child.id.clone(), kind: Kind::Subagent, session_id: parent.id.clone(), title: description.to_string(), started: Instant::now(), state: State::Running, out: None, stop: stop.clone() });
        let (eng, parent_id, child, description) = (self.clone(), parent.id.clone(), child.clone(), description.to_string());
        std::thread::spawn(move || {
            let result = eng.finish_subagent(&child, &Unattended, &stop);
            let (state, text) = match result {
                Ok(r) => (State::Finished, format!("Background subagent finished (sessionID: {}, task: {description}):\n\n{}", child.id, r.output)),
                Err(ToolError::Interrupted) => (State::Stopped, String::new()),
                Err(e) => (State::Failed(e.message()), format!("Background subagent failed (sessionID: {}, task: {description}): {}", child.id, e.message())),
            };
            eng.set_state(&child.id, state.clone());
            if state != State::Stopped {
                eng.tell_session(&parent_id, &text, &format!("background subagent {} {}", child.id, describe(&state)));
            }
            eng.notify(&format!("background subagent {description} {}", describe(&state)));
        });
    }

    /// The user's own `!command`: run it now in the session's directory (no model, no permission prompt), and record
    /// the exchange in the conversation so the model knows what happened. Returns the text to show.
    pub fn run_user_command(&self, sid: &str, command: &str, cancel: &AtomicBool) -> Result<String, String> {
        let session = self.store().session(sid).map_err(|e| e.to_string())?.ok_or("no such session")?;
        let f = shell::run_foreground(command, Path::new(&session.directory), &self.spill_dir(), 120_000, cancel, None).map_err(|e| e.message())?;
        if f.killed {
            return Err("interrupted".into());
        }
        let shown = if f.text.trim().is_empty() { "(no output)".to_string() } else { f.text };
        let _ = self.store().add_message(sid, "synthetic", &json!({"text": format!("The user ran this command themselves:\n$ {command}\n\n{shown}"), "description": "user shell command"}));
        Ok(shown)
    }
}
