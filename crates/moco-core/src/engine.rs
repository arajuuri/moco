//! Session engine: the step loop (docs/03-agent-loop.md).
//!
//! `run` claims the session, then repeats: deliver inbox items allowed at this boundary, run one step
//! (one LLM call, tools executing concurrently as their calls complete), continue while the step
//! produced tool calls. History is reloaded from the store on every step.
use crate::agent;
use crate::chat;
use crate::permission::{self, Effect, Rule};
use crate::provider::{retry, Block, ChatMessage, Event, FinishReason, Provider, ProviderError, Request, Role, ToolDef, Usage};
use crate::store::{Mode, Session, Store, StoreError};
use crate::tools::{self, project_root_of, PermReq, Permitter, Tool, ToolCtx, ToolError, ToolOutput};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Once,
    Always,
    /// Reject; with feedback the model continues and sees it, without feedback the run stops.
    Reject(Option<String>),
}

pub struct PermissionPrompt<'a> {
    pub action: &'a str,
    pub resources: &'a [String],
    pub save: &'a [String],
}

pub enum UiEvent<'a> {
    TextDelta(&'a str),
    ReasoningDelta(&'a str),
    ToolStarted { name: &'a str, input: &'a Value },
    ToolFinished { name: &'a str, ok: bool, text: &'a str },
    Retry { attempt: u32, wait: Duration, error: &'a str },
    Usage(Usage),
    Notice(&'a str),
}

/// Implemented by the front end. `ask` may be called from several tool threads; implementations
/// should serialize prompts (the engine already does).
pub trait Ui: Sync {
    fn event(&self, e: &UiEvent);
    fn ask(&self, p: &PermissionPrompt) -> Reply;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Idle,
    Interrupted,
    /// A tool call was declined without feedback.
    Declined,
    Failed(String),
    /// Another runner already holds the session.
    Busy,
}

pub struct EngineConfig {
    pub model: String,
    pub max_tokens: u32,
    pub app_dir: PathBuf,
    /// Waits for retry backoff; must return early when `cancel` is set.
    pub sleep: Box<dyn Fn(Duration, &AtomicBool) + Send + Sync>,
    pub jitter: Box<dyn Fn() -> f64 + Send + Sync>,
}

impl EngineConfig {
    pub fn new(model: &str, app_dir: PathBuf) -> Self {
        EngineConfig {
            model: model.into(),
            max_tokens: 16_000,
            app_dir,
            sleep: Box::new(|d, cancel| {
                let end = std::time::Instant::now() + d;
                while std::time::Instant::now() < end && !cancel.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }),
            jitter: Box::new(fastrand::f64),
        }
    }
}

pub struct Engine {
    pub store: Mutex<Store>,
    provider: Box<dyn Provider>,
    cfg: EngineConfig,
    tools: Vec<Box<dyn Tool>>,
}

enum StepOutcome {
    Continue,
    Done,
    Stop(RunOutcome),
}

struct AttemptResult {
    parts: Vec<Value>,
    stream: Result<(), ProviderError>,
    finish: Option<(FinishReason, Usage)>,
    dispatched: bool,
    declined: bool,
}

impl Engine {
    pub fn new(store: Store, provider: Box<dyn Provider>, cfg: EngineConfig) -> Self {
        Engine { store: Mutex::new(store), provider, cfg, tools: tools::all() }
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn spill_dir(&self) -> PathBuf {
        self.cfg.app_dir.join("data/tool-output")
    }

    /// Durably admit a user prompt (steered by default: delivered at the next step boundary).
    pub fn submit(&self, session_id: &str, text: &str, mode: Mode) -> Result<String, StoreError> {
        self.store().enqueue(session_id, None, mode, "user", &json!({ "text": text }))
    }

    /// Run until idle. `resume` continues a run that was interrupted by a crash (the claim is still held).
    pub fn run(&self, session_id: &str, ui: &dyn Ui, cancel: &AtomicBool, resume: bool) -> Result<RunOutcome, StoreError> {
        if !resume && !self.store().claim(session_id)? {
            return Ok(RunOutcome::Busy);
        }
        let outcome = self.drain(session_id, ui, cancel, resume);
        self.store().release(session_id)?;
        outcome
    }

    fn drain(&self, sid: &str, ui: &dyn Ui, cancel: &AtomicBool, resume: bool) -> Result<RunOutcome, StoreError> {
        let (mut continuing, mut step) = (resume, 0u32);
        loop {
            let delivered = self.store().deliver(sid, !continuing)?;
            if !delivered.is_empty() {
                step = 0; // new user input resets the step allowance
            }
            if !continuing && delivered.is_empty() {
                return Ok(RunOutcome::Idle);
            }
            if cancel.load(Ordering::Relaxed) {
                return Ok(RunOutcome::Interrupted);
            }
            match self.run_step(sid, step, ui, cancel)? {
                StepOutcome::Continue => continuing = true,
                StepOutcome::Done => continuing = false,
                StepOutcome::Stop(o) => return Ok(o),
            }
            step += 1;
        }
    }

    fn run_step(&self, sid: &str, step: u32, ui: &dyn Ui, cancel: &AtomicBool) -> Result<StepOutcome, StoreError> {
        let (session, history) = {
            let st = self.store();
            (st.session(sid)?.expect("session exists"), st.messages(sid)?)
        };
        let agent = agent::find(&session.agent).unwrap_or_else(|| agent::find("build").unwrap());
        let advertised: Vec<&dyn Tool> = self.tools.iter().map(|t| t.as_ref()).filter(|t| agent::advertised(&agent, *t)).collect();

        let mut messages = chat::to_chat(&history);
        let mut reminders = vec![];
        if agent.id == "plan" {
            reminders.push(agent::PLAN_REMINDER.to_string());
        }
        let max_steps_hit = agent.steps.is_some_and(|n| step >= n);
        if max_steps_hit {
            reminders.push(agent::MAX_STEPS.to_string());
        }
        for r in reminders {
            // Appended at request time (never stored), so it cannot be lost to compaction or revert.
            match messages.last_mut().filter(|m| m.role == Role::User) {
                Some(m) => m.content.push(Block::Text(r)),
                None => messages.push(ChatMessage { role: Role::User, content: vec![Block::Text(r)] }),
            }
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let req = Request {
            model: self.cfg.model.clone(),
            system: agent::system_prompt(&agent, &advertised, &agent::EnvInfo { cwd: &session.directory, platform: std::env::consts::OS, date: &agent::date_from_unix(now) }),
            messages,
            tools: advertised.iter().map(|t| ToolDef { name: t.name().into(), description: t.description().into(), schema: t.schema() }).collect(),
            max_tokens: self.cfg.max_tokens,
            tools_disabled: max_steps_hit,
        };

        let msg_id = self.store().add_message(sid, "assistant", &json!({ "parts": [] }))?;
        let declined = AtomicBool::new(false);
        let permitter = EnginePermitter {
            rules: agent.permissions.clone(),
            project_id: session.project_id.clone(),
            store: &self.store,
            ui,
            declined: &declined,
            ask_lock: Mutex::new(()),
        };

        let mut retries = 0u32;
        let result = loop {
            let r = self.attempt(&session, &req, &permitter, ui, cancel, &msg_id);
            match &r.stream {
                Err(e) if !r.dispatched && !cancel.load(Ordering::Relaxed) => {
                    // Safe to retry: no tool has run, and the partial output is discarded.
                    retries += 1;
                    if let Some(wait) = retry::delay(retries, e, (self.cfg.jitter)()) {
                        ui.event(&UiEvent::Retry { attempt: retries, wait, error: &e.message() });
                        self.store().update_message(&msg_id, &json!({ "parts": [] }))?;
                        (self.cfg.sleep)(wait, cancel);
                        if cancel.load(Ordering::Relaxed) {
                            break r;
                        }
                        continue;
                    }
                    break r;
                }
                _ => break r,
            }
        };

        let tool_calls = result.parts.iter().filter(|p| p["type"] == "tool").count();
        let interrupted = cancel.load(Ordering::Relaxed) || matches!(result.stream, Err(ProviderError::Interrupted));
        let mut data = json!({ "parts": result.parts });
        if let Some((reason, usage)) = &result.finish {
            data["finish"] = json!(format!("{reason:?}"));
            data["usage"] = json!({"input": usage.input, "output": usage.output, "cacheRead": usage.cache_read, "cacheWrite": usage.cache_write});
            ui.event(&UiEvent::Usage(*usage));
        }
        let outcome = match &result.stream {
            _ if interrupted => {
                data["finish"] = json!("interrupted");
                StepOutcome::Stop(RunOutcome::Interrupted)
            }
            Err(e) => {
                data["error"] = json!(e.message());
                ui.event(&UiEvent::Notice(&format!("error: {}", e.message())));
                StepOutcome::Stop(RunOutcome::Failed(e.message()))
            }
            Ok(()) if result.declined => StepOutcome::Stop(RunOutcome::Declined),
            Ok(()) => {
                if matches!(result.finish, Some((FinishReason::MaxTokens, _))) && tool_calls == 0 {
                    ui.event(&UiEvent::Notice("response stopped at the output token limit"));
                }
                if tool_calls > 0 { StepOutcome::Continue } else { StepOutcome::Done }
            }
        };
        self.store().update_message(&msg_id, &data)?;
        Ok(outcome)
    }

    /// One streaming request. Tools start executing as soon as their call is complete.
    fn attempt(&self, session: &Session, req: &Request, permitter: &EnginePermitter, ui: &dyn Ui, cancel: &AtomicBool, msg_id: &str) -> AttemptResult {
        let cwd = PathBuf::from(&session.directory);
        let root = project_root_of(&cwd);
        let spill = self.spill_dir();
        let mut parts: Vec<Value> = vec![];
        let mut text_idx: Option<usize> = None;
        let mut finish = None;
        let mut dispatched = false;
        let mut declined = false;

        let stream = std::thread::scope(|s| {
            let mut handles = vec![];
            let res = self.provider.stream(req, cancel, &mut |ev| match ev {
                Event::TextDelta(t) => {
                    ui.event(&UiEvent::TextDelta(&t));
                    let i = *text_idx.get_or_insert_with(|| {
                        parts.push(json!({"type":"text","text":""}));
                        parts.len() - 1
                    });
                    let cur = parts[i]["text"].as_str().unwrap_or("").to_string();
                    parts[i]["text"] = json!(cur + &t);
                }
                Event::ReasoningDelta(t) => ui.event(&UiEvent::ReasoningDelta(&t)),
                Event::ToolInputStart { .. } => text_idx = None,
                Event::ToolCall { id, name, input } => {
                    text_idx = None;
                    dispatched = true;
                    ui.event(&UiEvent::ToolStarted { name: &name, input: &input });
                    parts.push(chat::tool_part(&id, &name, &input));
                    let idx = parts.len() - 1;
                    let _ = self.store().update_message(msg_id, &json!({ "parts": parts }));
                    let tool = self.tools.iter().find(|t| t.name() == name).map(|t| t.as_ref());
                    let (cwd, root, spill) = (cwd.clone(), root.clone(), spill.clone());
                    handles.push((
                        idx,
                        s.spawn(move || match tool {
                            None => Err(ToolError::fail(format!("Unknown tool: {name}"))),
                            Some(t) => t.run(&input, &ToolCtx { cwd, project_root: root, spill_dir: spill, permitter, cancel }),
                        }),
                    ));
                }
                Event::Finish { reason, usage } => finish = Some((reason, usage)),
            });
            for (idx, h) in handles {
                let outcome = h.join().unwrap_or_else(|_| Err(ToolError::fail("tool panicked")));
                declined |= outcome == Err(ToolError::Declined);
                let name = parts[idx]["name"].as_str().unwrap_or("").to_string();
                let state = self.finish_tool(&name, outcome, &spill, ui);
                parts[idx]["state"] = state;
            }
            res
        });
        AttemptResult { parts, stream, finish, dispatched, declined }
    }

    fn finish_tool(&self, name: &str, outcome: Result<ToolOutput, ToolError>, spill: &std::path::Path, ui: &dyn Ui) -> Value {
        match outcome {
            Ok(out) => {
                let text = crate::truncate::truncate(&out.text, spill).map(|t| t.text).unwrap_or(out.text);
                let text = if text.is_empty() { "(no output)".to_string() } else { text };
                ui.event(&UiEvent::ToolFinished { name, ok: true, text: &text });
                json!({"status":"completed","output":text,"metadata":out.metadata})
            }
            Err(e) => {
                let msg = if e == ToolError::Interrupted { chat::INTERRUPTED.to_string() } else { e.message() };
                ui.event(&UiEvent::ToolFinished { name, ok: false, text: &msg });
                json!({"status":"error","error":msg})
            }
        }
    }
}

/// Rule evaluation + user prompting for tool permission requests.
struct EnginePermitter<'a> {
    rules: Vec<Rule>,
    project_id: String,
    store: &'a Mutex<Store>,
    ui: &'a dyn Ui,
    /// Set when the user declines without feedback: every other pending/future request of the batch fails too.
    declined: &'a AtomicBool,
    ask_lock: Mutex<()>,
}

impl EnginePermitter<'_> {
    fn effect(&self, req: &PermReq) -> Effect {
        let saved = self.store.lock().unwrap_or_else(|e| e.into_inner()).saved_permissions(&self.project_id).unwrap_or_default();
        let e = permission::decide(&req.action, &req.resources, &self.rules, &saved);
        if e == Effect::Allow && req.force_ask { Effect::Ask } else { e }
    }
}

impl Permitter for EnginePermitter<'_> {
    fn check(&self, req: &PermReq) -> Result<(), ToolError> {
        if self.declined.load(Ordering::Relaxed) {
            return Err(ToolError::Declined);
        }
        match self.effect(req) {
            Effect::Allow => Ok(()),
            Effect::Deny => Err(ToolError::Blocked(format!("Permission denied: {} {}", req.action, req.resources.join(", ")))),
            Effect::Ask => {
                let _one_prompt_at_a_time = self.ask_lock.lock().unwrap_or_else(|e| e.into_inner());
                if self.declined.load(Ordering::Relaxed) {
                    return Err(ToolError::Declined);
                }
                // An "always" answer to an earlier prompt may already cover this request.
                if self.effect(req) == Effect::Allow && !req.force_ask {
                    return Ok(());
                }
                match self.ui.ask(&PermissionPrompt { action: &req.action, resources: &req.resources, save: &req.save }) {
                    Reply::Once => Ok(()),
                    Reply::Always => {
                        let st = self.store.lock().unwrap_or_else(|e| e.into_inner());
                        for pat in &req.save {
                            let _ = st.save_permission(&self.project_id, &req.action, pat);
                        }
                        Ok(())
                    }
                    Reply::Reject(None) => {
                        self.declined.store(true, Ordering::Relaxed);
                        Err(ToolError::Declined)
                    }
                    Reply::Reject(Some(feedback)) => Err(ToolError::Corrected(feedback)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::fake::*;
    use crate::provider::TransportKind;
    use std::collections::VecDeque;
    use std::sync::Arc;

    #[derive(Default)]
    struct TestUi {
        log: Mutex<Vec<String>>,
        replies: Mutex<VecDeque<Reply>>,
        asked: Mutex<Vec<String>>,
        hook: Mutex<Option<Box<dyn Fn(&str) + Send>>>,
    }
    impl TestUi {
        fn replying(r: Vec<Reply>) -> Self {
            TestUi { replies: Mutex::new(r.into()), ..Default::default() }
        }
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }
    impl Ui for TestUi {
        fn event(&self, e: &UiEvent) {
            let line = match e {
                UiEvent::TextDelta(t) => format!("text:{t}"),
                UiEvent::ReasoningDelta(_) => "reasoning".into(),
                UiEvent::ToolStarted { name, .. } => format!("start:{name}"),
                UiEvent::ToolFinished { name, ok, .. } => format!("done:{name}:{ok}"),
                UiEvent::Retry { attempt, wait, .. } => format!("retry:{attempt}:{}", wait.as_secs()),
                UiEvent::Usage(_) => "usage".into(),
                UiEvent::Notice(n) => format!("notice:{n}"),
            };
            if let Some(h) = &*self.hook.lock().unwrap() {
                h(&line);
            }
            self.log.lock().unwrap().push(line);
        }
        fn ask(&self, p: &PermissionPrompt) -> Reply {
            self.asked.lock().unwrap().push(format!("{} {}", p.action, p.resources.join(",")));
            self.replies.lock().unwrap().pop_front().expect("unexpected permission prompt")
        }
    }

    struct Fixture {
        engine: Arc<Engine>,
        fake: Arc<FakeProvider>,
        dir: PathBuf,
        session: Session,
        waits: Arc<Mutex<Vec<Duration>>>,
    }
    struct Shared(Arc<FakeProvider>);
    impl Provider for Shared {
        fn stream(&self, r: &Request, c: &AtomicBool, f: &mut dyn FnMut(Event)) -> Result<(), ProviderError> {
            self.0.stream(r, c, f)
        }
    }

    fn fixture(script: Vec<Scripted>) -> Fixture {
        let dir = std::env::temp_dir().join(crate::id::new_id("moco-eng"));
        std::fs::create_dir_all(dir.join("work")).unwrap();
        let fake = Arc::new(FakeProvider::new(script));
        let waits = Arc::new(Mutex::new(vec![]));
        let mut cfg = EngineConfig::new("test-model", dir.join("app"));
        let w = waits.clone();
        cfg.sleep = Box::new(move |d, _| w.lock().unwrap().push(d));
        cfg.jitter = Box::new(|| 0.0);
        let store = Store::open_memory().unwrap();
        let session = store.create_session(dir.join("work").to_str().unwrap(), "build").unwrap();
        let engine = Arc::new(Engine::new(store, Box::new(Shared(fake.clone())), cfg));
        Fixture { engine, fake, dir, session, waits }
    }
    impl Fixture {
        fn say(&self, text: &str) {
            self.engine.submit(&self.session.id, text, Mode::Steer).unwrap();
        }
        fn run(&self, ui: &TestUi) -> RunOutcome {
            self.engine.run(&self.session.id, ui, &AtomicBool::new(false), false).unwrap()
        }
        fn msgs(&self) -> Vec<crate::store::Message> {
            self.engine.store().messages(&self.session.id).unwrap()
        }
        fn file(&self, rel: &str, content: &str) {
            std::fs::write(self.dir.join("work").join(rel), content).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn last_user_blocks(req: &Request) -> Vec<Block> {
        req.messages.last().unwrap().content.clone()
    }

    #[test]
    fn plain_turn_persists_and_goes_idle() {
        let f = fixture(vec![text("hello there")]);
        f.say("hi");
        let ui = TestUi::default();
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        let m = f.msgs();
        assert_eq!((m[0].kind.as_str(), m[0].data["text"].as_str()), ("user", Some("hi")));
        assert_eq!(m[1].data["parts"][0]["text"], "hello there");
        assert_eq!(m[1].data["usage"]["output"], 5);
        assert_eq!(ui.log(), ["text:hello there", "usage"]);
        assert!(f.engine.store().claim(&f.session.id).unwrap(), "claim released");
        let req = &f.fake.requests.lock().unwrap()[0];
        assert!(req.system.contains("moco") && req.system.contains("Working directory"));
        assert_eq!(req.tools.len(), 6);
    }

    #[test]
    fn tool_call_result_flows_back_to_the_model() {
        let f = fixture(vec![tool_call("t1", "read", json!({"path":"a.txt"})), text("it says hi")]);
        f.file("a.txt", "hi\n");
        f.say("read a.txt");
        let ui = TestUi::default();
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(last_user_blocks(&reqs[1]), [Block::ToolResult { id: "t1".into(), content: "1: hi".into(), is_error: false }]);
        let m = f.msgs();
        assert_eq!(m[1].data["parts"][0]["state"]["status"], "completed");
        assert_eq!(ui.log(), ["start:read", "done:read:true", "usage", "text:it says hi", "usage"]);
    }

    #[test]
    fn parallel_tool_calls_all_complete_before_the_next_step() {
        let f = fixture(vec![
            Scripted::Events(vec![
                Event::ToolCall { id: "a".into(), name: "read".into(), input: json!({"path":"x"}) },
                Event::ToolCall { id: "b".into(), name: "shell".into(), input: json!({"command":"echo two"}) },
                Event::Finish { reason: FinishReason::ToolUse, usage: Usage::default() },
            ]),
            text("done"),
        ]);
        f.file("x", "one");
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        let results = last_user_blocks(&reqs[1]);
        assert_eq!(results.len(), 2);
        assert!(matches!(&results[1], Block::ToolResult { content, .. } if content == "two\n"));
    }

    #[test]
    fn unknown_tool_and_bad_input_are_model_visible_errors() {
        let f = fixture(vec![tool_call("t1", "nope", json!({})), tool_call("t2", "read", json!({})), text("ok")]);
        f.say("x");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert!(matches!(&last_user_blocks(&reqs[1])[0], Block::ToolResult { content, is_error: true, .. } if content == "Unknown tool: nope"));
        assert!(matches!(&last_user_blocks(&reqs[2])[0], Block::ToolResult { content, is_error: true, .. } if content.contains("`path` is required")));
    }

    #[test]
    fn declined_without_feedback_stops_the_run() {
        let f = fixture(vec![tool_call("t1", "read", json!({"path":".env"}))]);
        f.file(".env", "SECRET=1");
        f.say("read env");
        let ui = TestUi::replying(vec![Reply::Reject(None)]);
        assert_eq!(f.run(&ui), RunOutcome::Declined);
        assert_eq!(ui.asked.lock().unwrap().as_slice(), ["read .env"]);
        assert_eq!(f.fake.calls(), 1, "no further model call");
        assert_eq!(f.msgs()[1].data["parts"][0]["state"]["status"], "error");
    }

    #[test]
    fn declined_with_feedback_continues_and_always_is_remembered() {
        let f = fixture(vec![
            tool_call("t1", "read", json!({"path":".env"})),
            tool_call("t2", "read", json!({"path":".env"})),
            tool_call("t3", "read", json!({"path":".env"})),
            text("done"),
        ]);
        f.file(".env", "SECRET=1");
        f.say("go");
        let ui = TestUi::replying(vec![Reply::Reject(Some("use .env.example".into())), Reply::Always]);
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert!(matches!(&last_user_blocks(&reqs[1])[0], Block::ToolResult { content, is_error: true, .. } if content.contains("use .env.example")));
        assert!(matches!(&last_user_blocks(&reqs[3])[0], Block::ToolResult { content, is_error: false, .. } if content == "1: SECRET=1"));
        assert_eq!(ui.asked.lock().unwrap().len(), 2, "third read covered by the saved rule");
        assert_eq!(f.engine.store().saved_permissions(&f.session.project_id).unwrap().len(), 1);
    }

    #[test]
    fn plan_agent_hides_edit_tools_and_blocks_them() {
        let f = fixture(vec![tool_call("t1", "write", json!({"path":"x","content":"c"})), text("ok")]);
        f.engine.store().set_agent(&f.session.id, "plan").unwrap();
        f.say("make a file");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert!(!reqs[0].tools.iter().any(|t| t.name == "write" || t.name == "edit"));
        assert!(last_user_blocks(&reqs[0]).iter().any(|b| matches!(b, Block::Text(t) if t.contains("Plan mode"))));
        assert!(matches!(&last_user_blocks(&reqs[1])[0], Block::ToolResult { content, is_error: true, .. } if content.starts_with("Permission denied: edit")));
        assert!(!f.dir.join("work/x").exists());
        // The reminder is request-time only, never stored.
        assert!(f.msgs()[0].data["text"].as_str().unwrap() == "make a file");
    }

    #[test]
    fn retries_discard_partial_output_and_report_waits() {
        let f = fixture(vec![
            Scripted::Fail(vec![Event::TextDelta("par".into())], ProviderError::Internal { retry_after_ms: None, message: "overloaded".into() }),
            Scripted::Fail(vec![], ProviderError::RateLimit { retry_after_ms: Some(30_000), message: "slow".into() }),
            text("final"),
        ]);
        f.say("hi");
        let ui = TestUi::default();
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        assert_eq!(*f.waits.lock().unwrap(), [Duration::from_secs(2), Duration::from_secs(30)]);
        assert_eq!(f.msgs()[1].data["parts"], json!([{"type":"text","text":"final"}]));
        assert!(ui.log().contains(&"retry:1:2".to_string()) && ui.log().contains(&"retry:2:30".to_string()));
    }

    #[test]
    fn non_retryable_error_surfaces_and_is_recorded() {
        let f = fixture(vec![Scripted::Fail(vec![], ProviderError::Auth("bad key".into()))]);
        f.say("hi");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Failed("bad key".into()));
        assert_eq!(f.msgs()[1].data["error"], "bad key");
        assert_eq!(f.fake.calls(), 1);
    }

    #[test]
    fn retries_give_up_after_the_limit() {
        let err = || Scripted::Fail(vec![], ProviderError::Transport { kind: TransportKind::Other, message: "reset".into() });
        let f = fixture((0..11).map(|_| err()).collect());
        f.say("hi");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Failed("reset".into()));
        assert_eq!(f.fake.calls(), 11, "1 attempt + 10 retries");
    }

    #[test]
    fn a_failure_after_a_tool_ran_is_not_retried() {
        let f = fixture(vec![Scripted::Fail(
            vec![Event::ToolCall { id: "t".into(), name: "shell".into(), input: json!({"command":"touch ran"}) }],
            ProviderError::Internal { retry_after_ms: None, message: "dropped".into() },
        )]);
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Failed("dropped".into()));
        assert_eq!(f.fake.calls(), 1, "re-running would repeat the side effect");
        assert!(f.dir.join("work/ran").exists());
        assert_eq!(f.msgs()[1].data["parts"][0]["state"]["status"], "completed");
    }

    #[test]
    fn interrupt_stops_the_run_and_keeps_history_valid() {
        let f = fixture(vec![Scripted::Events(vec![
            Event::TextDelta("a".into()),
            Event::ToolCall { id: "t".into(), name: "shell".into(), input: json!({"command":"sleep 30"}) },
            Event::TextDelta("b".into()),
            Event::Finish { reason: FinishReason::ToolUse, usage: Usage::default() },
        ])]);
        f.say("go");
        let cancel = Arc::new(AtomicBool::new(false));
        let ui = TestUi::default();
        let c = cancel.clone();
        *ui.hook.lock().unwrap() = Some(Box::new(move |l| {
            if l == "text:b" {
                c.store(true, Ordering::Relaxed);
            }
        }));
        let t = std::time::Instant::now();
        assert_eq!(f.engine.run(&f.session.id, &ui, &cancel, false).unwrap(), RunOutcome::Interrupted);
        assert!(t.elapsed() < Duration::from_secs(10));
        let m = f.msgs();
        assert_eq!(m[1].data["finish"], "interrupted");
        assert_eq!(m[1].data["parts"][1]["state"]["error"], chat::INTERRUPTED);
        // The next request must still be well-formed: every tool_use has a result.
        let chat = chat::to_chat(&m);
        assert!(matches!(&chat[2].content[0], Block::ToolResult { is_error: true, .. }));
    }

    #[test]
    fn steered_prompt_is_delivered_at_the_next_step_boundary() {
        let f = fixture(vec![tool_call("t1", "shell", json!({"command":"true"})), text("saw both")]);
        f.say("first");
        let ui = TestUi::default();
        let engine = f.engine.clone();
        let sid = f.session.id.clone();
        *ui.hook.lock().unwrap() = Some(Box::new(move |l| {
            if l == "done:shell:true" {
                engine.submit(&sid, "actually, also this", Mode::Steer).unwrap();
            }
        }));
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        let tail = last_user_blocks(&reqs[1]);
        assert!(matches!(&tail[0], Block::ToolResult { .. }));
        assert!(matches!(&tail[1], Block::Text(t) if t == "actually, also this"));
    }

    #[test]
    fn queued_prompt_waits_for_idle_then_runs() {
        let f = fixture(vec![text("one"), text("two")]);
        f.say("first");
        f.engine.submit(&f.session.id, "second", Mode::Queue).unwrap();
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let kinds: Vec<_> = f.msgs().iter().map(|m| m.kind.clone()).collect();
        assert_eq!(kinds, ["user", "assistant", "user", "assistant"]);
        assert_eq!(f.fake.calls(), 2);
    }

    #[test]
    fn resume_after_crash_repairs_dangling_tool_calls() {
        let f = fixture(vec![text("recovered")]);
        {
            let st = f.engine.store();
            st.add_message(&f.session.id, "user", &json!({"text":"do it"})).unwrap();
            st.add_message(&f.session.id, "assistant", &json!({"parts":[{"type":"tool","id":"t1","name":"shell","input":{"command":"x"},"state":{"status":"running"}}]})).unwrap();
            assert!(st.claim(&f.session.id).unwrap());
            assert_eq!(st.recover_claims(3).unwrap(), [f.session.id.clone()]);
        }
        let ui = TestUi::default();
        assert_eq!(f.engine.run(&f.session.id, &ui, &AtomicBool::new(false), true).unwrap(), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert!(matches!(&last_user_blocks(&reqs[0])[0], Block::ToolResult { content, is_error: true, .. } if content == chat::INTERRUPTED));
    }

    #[test]
    fn busy_session_is_not_run_twice() {
        let f = fixture(vec![]);
        assert!(f.engine.store().claim(&f.session.id).unwrap());
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Busy);
    }
}
