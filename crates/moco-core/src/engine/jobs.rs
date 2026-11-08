//! Background jobs: shell commands and subagents that keep running after the tool call that started them returned.
//! When one ends, its result is queued as a synthetic message in the owning session (delivered at the next step
//! boundary of a running turn, or with the next prompt), and the UI is told through the notifier.
use crate::compat::*;
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
    /// A spending limit stopped a background subagent (what was exceeded).
    LimitReached(String),
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
    limits: Mutex<crate::config::JobLimits>,
}

/// Waits before the second and third attempt to queue a job's result.
const TELL_BACKOFF: [Duration; 2] = [Duration::from_millis(100), Duration::from_millis(300)];

/// `list` shows this many finished jobs; older ones stay inspectable by id (up to `KEEP_FINISHED`).
const LISTED_FINISHED: usize = 60;
const KEEP_FINISHED: usize = 1000;

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
        State::LimitReached(what) => format!("stopped at a spending limit: {what}"),
    }
}

/// Answers to permission questions from a background subagent: nobody is at the keyboard for it.
struct Unattended;

impl Ui for Unattended {
    fn event(&self, _: &UiEvent) {}
    fn unattended(&self) -> bool {
        true
    }
    fn ask(&self, _: &PermissionPrompt) -> Reply {
        Reply::Reject(Some("This needs approval, but you are a subagent running in the background and nobody is present to give it. Do not retry it; do what is possible without it and say what needs approval.".into()))
    }
}

fn slot_result(kind: Kind, running: usize, limit: usize) -> Result<(), ToolError> {
    if running < limit {
        return Ok(());
    }
    let what = if kind == Kind::Shell { "background shell commands" } else { "background subagents" };
    Err(ToolError::fail(format!("Too many {what} are running ({running} of {limit} allowed). Check them with the jobs tool and stop one with kill_job, or wait for one to finish, before starting another.")))
}

impl Engine {
    /// Put the engine behind an `Arc` and give it a handle to itself, which background jobs need.
    pub fn share(self) -> Arc<Engine> {
        let a = Arc::new(self);
        *a.me.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(&a);
        a.prune_spill_in_background();
        a
    }

    /// Delete week-old tool output and job logs, except those of running jobs, on a thread of its own.
    fn prune_spill_in_background(self: &Arc<Self>) {
        let (weak, dir) = (Arc::downgrade(self), self.spill_dir());
        std::thread::spawn(move || {
            let n = crate::truncate::prune_spill_dir(&dir, crate::truncate::SPILL_MAX_AGE, &|p| weak.upgrade().is_some_and(|e| e.jobs_lock().iter().any(|j| j.state == State::Running && j.out.as_deref() == Some(p))));
            if n > 0 {
                crate::log_info!("jobs", "pruned {n} old tool-output files");
            }
        });
    }

    /// Replace the caps on concurrently running background jobs.
    pub fn set_job_limits(&self, limits: crate::config::JobLimits) {
        *self.jobs.limits.lock().unwrap_or_else(|e| e.into_inner()) = limits;
    }

    fn job_limit(&self, kind: Kind) -> usize {
        let l = *self.jobs.limits.lock().unwrap_or_else(|e| e.into_inner());
        if kind == Kind::Shell { l.max_shells } else { l.max_subagents }
    }

    /// Tool error unless another job of `kind` may start now.
    pub fn check_background_slot(&self, subagent: bool) -> Result<(), ToolError> {
        let kind = if subagent { Kind::Subagent } else { Kind::Shell };
        let running = self.jobs_lock().iter().filter(|j| j.kind == kind && j.state == State::Running).count();
        slot_result(kind, running, self.job_limit(kind))
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

    /// Add a running job unless its kind is at the cap (checked and added under one lock, so concurrent starts
    /// cannot both slip through).
    fn register(&self, job: Job) -> Result<(), ToolError> {
        let limit = self.job_limit(job.kind);
        let mut list = self.jobs_lock();
        if job.state == State::Running {
            slot_result(job.kind, list.iter().filter(|j| j.kind == job.kind && j.state == State::Running).count(), limit)?;
        }
        list.push(job);
        // Bound memory: forget the oldest finished jobs. `background_jobs` lists fewer than are kept, so an id
        // the model was given stays inspectable long after it scrolled out of the list.
        while list.len() > KEEP_FINISHED {
            match list.iter().position(|j| j.state != State::Running) {
                Some(i) => {
                    list.remove(i);
                }
                None => break,
            }
        }
        Ok(())
    }

    fn unregister(&self, id: &str) {
        self.jobs_lock().retain(|j| j.id != id);
    }

    fn set_state(&self, id: &str, state: State) {
        if let Some(j) = self.jobs_lock().iter_mut().find(|j| j.id == id) {
            j.state = state;
        }
    }

    /// Queue `text` for the session as a message the model will see. A busy database (another moco process holding
    /// a write lock) is retried a few times; if it still fails the user is told through the notifier, with `lost` (what
    /// the model will not hear about, ideally the job id and where its output is) so the result is not lost silently.
    fn tell_session(&self, sid: &str, text: &str, description: &str, lost: &str) {
        let data = json!({"text": text, "description": description});
        let mut last = None;
        for (attempt, wait) in std::iter::once(Duration::ZERO).chain(TELL_BACKOFF).enumerate() {
            std::thread::sleep(wait);
            match self.store().enqueue(sid, None, Mode::Steer, "synthetic", &data) {
                Ok(_) => return,
                Err(e) => {
                    crate::log_warn!("store", "queueing a background job's result failed (attempt {}): {e}", attempt + 1);
                    last = Some(e);
                }
            }
        }
        if let Some(e) = last {
            self.notify(&format!("warning: the model was not told that {lost}: {e}"));
        }
    }

    pub fn background_jobs(&self, session: Option<&str>) -> Vec<JobInfo> {
        let list = self.jobs_lock();
        // All running jobs, plus the most recent finished ones.
        let mut finished_from_end = 0;
        let mut shown: Vec<bool> = list
            .iter()
            .rev()
            .map(|j| {
                if j.state == State::Running {
                    return true;
                }
                finished_from_end += 1;
                finished_from_end <= LISTED_FINISHED
            })
            .collect();
        shown.reverse();
        list.iter()
            .zip(shown)
            .filter(|(_, s)| *s)
            .map(|(j, _)| j)
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
        crate::log_info!("jobs", "kill requested job={id}");
        stop.store(true, Ordering::Relaxed);
        let end = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end && self.jobs_lock().iter().any(|j| j.id == id && j.state == State::Running) {
            std::thread::sleep(Duration::from_millis(20));
        }
        if self.jobs_lock().iter().any(|j| j.id == id && j.state == State::Running) {
            crate::log_warn!("jobs", "job={id} was still running 3 s after the kill request");
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
                // Only the end of the log: a dev server can run for hours and write gigabytes.
                let text = crate::log::read_tail_bounded(&path, lines, 1024 * 1024).unwrap_or_default();
                let tail = text.trim_end_matches('\n');
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
        let stop = Arc::new(AtomicBool::new(false));
        // Reserve the slot first (the cap is enforced here), then start the process.
        self.register(Job { id: id.clone(), kind: Kind::Shell, session_id: session.id.clone(), title: command.to_string(), started: Instant::now(), state: State::Running, out: Some(out.clone()), stop: stop.clone() })?;
        let mut child = match shell::spawn(command, workdir, &out, root.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                self.unregister(&id);
                return Err(e);
            }
        };
        crate::log_info!("jobs", "start shell job={id} session={} sandboxed={sandboxed} timeout_ms={} command={}", session.id, timeout.map_or("none".to_string(), |t| t.as_millis().to_string()), classifier::sanitize(command, 200));
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
        let ran_ms = self.jobs_lock().iter().find(|j| j.id == id).map_or(0, |j| j.started.elapsed().as_millis());
        crate::log_info!("jobs", "finish shell job={id} session={sid} {what} ms={ran_ms}");
        self.set_state(id, state.clone());
        let output = if f.text.trim().is_empty() { "(no output)".to_string() } else { f.text.clone() };
        let out_path = self.jobs_lock().iter().find(|j| j.id == id).and_then(|j| j.out.as_ref().map(|p| p.display().to_string())).unwrap_or_else(|| "an unknown file".into());
        // Whoever stopped it knows; everything else is news for the model.
        if state != State::Stopped {
            self.tell_session(
                sid,
                &format!("Background command finished (shell ID: {id}): {what}.\nCommand: {command}\n\n{output}"),
                &format!("background shell {id} {what}"),
                &format!("background shell {id} {what} (its output is in {out_path})"),
            );
        }
        self.notify(&format!("background {id} `{}` {what}", command.lines().next().unwrap_or("").chars().take(60).collect::<String>()));
    }

    /// Run the prepared child session on a thread of its own, unattended; report its result to `parent`.
    pub(super) fn start_subagent_job(self: &Arc<Self>, parent: &Session, child: &Session, description: &str) {
        let stop = Arc::new(AtomicBool::new(false));
        crate::log_info!("jobs", "start subagent job={} session={} agent={} task={}", child.id, parent.id, child.agent, classifier::sanitize(description, 120));
        let job = Job { id: child.id.clone(), kind: Kind::Subagent, session_id: parent.id.clone(), title: description.to_string(), started: Instant::now(), state: State::Running, out: None, stop: stop.clone() };
        if let Err(e) = self.register(job) {
            // Safety net: the caller should have run `check_background_slot` before creating the child session.
            let msg = e.message();
            crate::log_warn!("jobs", "subagent job={} refused: {msg}", child.id);
            let failed = Job { id: child.id.clone(), kind: Kind::Subagent, session_id: parent.id.clone(), title: description.to_string(), started: Instant::now(), state: State::Failed(msg.clone()), out: None, stop };
            let _ = self.register(failed);
            self.tell_session(&parent.id, &format!("Background subagent not started (sessionID: {}, task: {description}): {msg}", child.id), &format!("background subagent {} refused", child.id), &format!("background subagent {} was refused ({description})", child.id));
            return;
        }
        let (eng, parent_id, child, description) = (self.clone(), parent.id.clone(), child.clone(), description.to_string());
        std::thread::spawn(move || {
            let result = eng.finish_subagent(&child, &Unattended, &stop);
            let (state, text) = match result {
                Ok(r) => (State::Finished, format!("Background subagent finished (sessionID: {}, task: {description}):\n\n{}", child.id, r.output)),
                Err(ToolError::Interrupted) => (State::Stopped, String::new()),
                Err(e) if e.message().starts_with(limits::SUBAGENT_STOPPED) => {
                    let what = e.message().trim_start_matches(limits::SUBAGENT_STOPPED).to_string();
                    let text = format!("Background subagent stopped at a spending limit (sessionID: {}, task: {description}): {what}. Its work so far is kept in that session. Tell the user; do not start it again unless they agree.", child.id);
                    (State::LimitReached(what), text)
                }
                Err(e) => (State::Failed(e.message()), format!("Background subagent failed (sessionID: {}, task: {description}): {}", child.id, e.message())),
            };
            crate::log_info!("jobs", "finish subagent job={} session={parent_id} {}", child.id, describe(&state));
            eng.set_state(&child.id, state.clone());
            if state != State::Stopped {
                eng.tell_session(&parent_id, &text, &format!("background subagent {} {}", child.id, describe(&state)), &format!("background subagent {} {} (its work is in session {})", child.id, describe(&state), child.id));
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
        let data = json!({"text": format!("The user ran this command themselves:\n$ {command}\n\n{shown}"), "description": "user shell command"});
        let mut last = None;
        for wait in std::iter::once(Duration::ZERO).chain(TELL_BACKOFF) {
            std::thread::sleep(wait);
            match self.store().add_message(sid, "synthetic", &data) {
                Ok(_) => {
                    last = None;
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        if let Some(e) = last {
            crate::log_warn!("store", "recording the user's own command failed: {e}");
            self.notify(&format!("warning: the model will not know you ran `{}`: {e}", command.lines().next().unwrap_or("").chars().take(60).collect::<String>()));
        }
        Ok(shown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::fake::FakeProvider;

    fn engine(limits: crate::config::JobLimits) -> (Arc<Engine>, Session, PathBuf) {
        let dir = std::env::temp_dir().join(crate::id::new_id("moco-jobs"));
        std::fs::create_dir_all(dir.join("work")).unwrap();
        let store = Store::open_memory().unwrap();
        let session = store.create_session(dir.join("work").to_str().unwrap(), "build").unwrap();
        let eng = Engine::new(store, Box::new(FakeProvider::new(vec![])), EngineConfig::new("test-model", dir.join("app"))).share();
        eng.set_job_limits(limits);
        (eng, session, dir)
    }

    fn collect_notices(eng: &Engine) -> Arc<Mutex<Vec<String>>> {
        let seen = Arc::new(Mutex::new(vec![]));
        let s = seen.clone();
        eng.set_notifier(Arc::new(move |l: &str| s.lock().unwrap().push(l.to_string())));
        seen
    }

    #[test]
    fn a_job_result_survives_a_store_that_fails_a_couple_of_times() {
        let (eng, s, dir) = engine(crate::config::JobLimits::default());
        let notices = collect_notices(&eng);
        eng.store().exec_for_test("alter table session_inbox rename to session_inbox_away");
        // The first attempt (at once) and the second (after 100 ms) fail; the third (after 400 ms) finds the table back.
        let fixer = {
            let eng = eng.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(250));
                eng.store().exec_for_test("alter table session_inbox_away rename to session_inbox");
            })
        };
        eng.tell_session(&s.id, "Background command finished", "background shell shl_x exited", "background shell shl_x exited (its output is in /tmp/x.txt)");
        fixer.join().unwrap();
        assert_eq!(eng.store().pending_inbox(&s.id).unwrap(), 1, "the result reached the inbox");
        assert!(notices.lock().unwrap().is_empty(), "no warning when a retry worked: {:?}", notices.lock().unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_job_result_that_cannot_be_queued_is_reported_with_the_job_id_and_output_path() {
        let (eng, s, dir) = engine(crate::config::JobLimits::default());
        let notices = collect_notices(&eng);
        eng.store().exec_for_test("alter table session_inbox rename to session_inbox_away");
        eng.tell_session(&s.id, "Background command finished", "background shell shl_x exited", "background shell shl_x exited (its output is in /tmp/x.txt)");
        let n = notices.lock().unwrap().clone();
        assert_eq!(n.len(), 1, "{n:?}");
        assert!(n[0].contains("shl_x") && n[0].contains("/tmp/x.txt") && n[0].contains("not told"), "{}", n[0]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_finished_shell_job_reports_its_output_path_when_the_store_is_broken() {
        let (eng, s, dir) = engine(crate::config::JobLimits::default());
        let notices = collect_notices(&eng);
        eng.store().exec_for_test("alter table session_inbox rename to session_inbox_away");
        let (id, out) = eng.start_shell_job(&s, "echo hi", &dir.join("work"), None, true).unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        while notices.lock().unwrap().len() < 2 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        let n = notices.lock().unwrap().clone();
        assert!(n.iter().any(|l| l.contains(&id) && l.contains(&out.display().to_string()) && l.contains("not told")), "{n:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_user_command_the_model_cannot_be_told_about_is_reported() {
        let (eng, s, dir) = engine(crate::config::JobLimits::default());
        let notices = collect_notices(&eng);
        eng.store().exec_for_test("alter table message rename to message_away");
        // `run_user_command` first reads the session, which still works; only the message insert fails.
        let shown = eng.run_user_command(&s.id, "echo hi", &AtomicBool::new(false)).unwrap();
        assert!(shown.contains("hi"));
        let n = notices.lock().unwrap().clone();
        assert!(n.len() == 1 && n[0].contains("will not know") && n[0].contains("echo hi"), "{n:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn shell_cap_is_a_tool_error_and_frees_up_when_a_job_stops() {
        let (eng, s, dir) = engine(crate::config::JobLimits { max_shells: 2, max_subagents: 4 });
        let work = dir.join("work");
        let a = eng.start_shell_job(&s, "sleep 31", &work, None, true).unwrap().0;
        eng.start_shell_job(&s, "sleep 32", &work, None, true).unwrap();
        let err = eng.start_shell_job(&s, "sleep 33", &work, None, true).unwrap_err().message();
        assert!(err.contains("Too many background shell commands") && err.contains("2 of 2"), "{err}");
        assert_eq!(eng.running_jobs(), 2);
        assert!(eng.check_background_slot(false).is_err());
        // Shells do not use up subagent slots.
        assert!(eng.check_background_slot(true).is_ok());
        eng.kill_job(&a).unwrap();
        assert!(eng.start_shell_job(&s, "sleep 34", &work, None, true).is_ok());
        eng.kill_all_jobs();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn subagent_cap_applies_to_subagents_only() {
        let (eng, _s, dir) = engine(crate::config::JobLimits { max_shells: 1, max_subagents: 0 });
        assert!(eng.check_background_slot(true).unwrap_err().message().contains("background subagents"));
        assert!(eng.check_background_slot(false).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn finished_jobs_stay_inspectable_after_they_leave_the_list() {
        let (eng, s, dir) = engine(crate::config::JobLimits::default());
        let mut first = String::new();
        for i in 0..(LISTED_FINISHED + 5) {
            let id = format!("shl_test{i:03}");
            if i == 0 {
                first = id.clone();
            }
            eng.register(Job { id, kind: Kind::Shell, session_id: s.id.clone(), title: format!("job {i}"), started: Instant::now(), state: State::Exited { code: Some(0), signal: None }, out: None, stop: Arc::new(AtomicBool::new(false)) }).unwrap();
        }
        let listed = eng.background_jobs(None);
        assert_eq!(listed.len(), LISTED_FINISHED);
        assert!(listed.iter().all(|j| j.id != first));
        assert!(eng.job_output(&first, 10).unwrap().contains("exited (code 0)"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sharing_the_engine_prunes_old_spill_files_in_the_background() {
        let dir = std::env::temp_dir().join(crate::id::new_id("moco-jobs"));
        let spill = dir.join("app").join("data").join("tool-output");
        std::fs::create_dir_all(&spill).unwrap();
        let old = spill.join("out_old.txt");
        std::fs::write(&old, "x").unwrap();
        std::fs::OpenOptions::new().write(true).open(&old).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(10 * 24 * 3600)).unwrap();
        let fresh = spill.join("out_fresh.txt");
        std::fs::write(&fresh, "y").unwrap();
        let store = Store::open_memory().unwrap();
        let eng = Engine::new(store, Box::new(FakeProvider::new(vec![])), EngineConfig::new("test-model", dir.join("app"))).share();
        assert_eq!(eng.spill_dir(), spill);
        let end = Instant::now() + Duration::from_secs(5);
        while old.exists() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!old.exists() && fresh.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
