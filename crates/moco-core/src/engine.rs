//! Session engine: the step loop (docs/03-agent-loop.md).
//!
//! `run` claims the session, then repeats: deliver inbox items allowed at this boundary, run one step
//! (one LLM call, tools executing concurrently as their calls complete), continue while the step
//! produced tool calls. History is reloaded from the store on every step.
use crate::agent::{self, Agent};
use crate::{catalog, compaction, config, instructions};
use crate::chat;
use crate::permission::{self, Effect, Rule};
use crate::provider::{retry, Block, ChatMessage, Event, FinishReason, Provider, ProviderError, Request, Role, ToolDef, Usage};
use crate::store::{Mode, Session, Store, StoreError};
use crate::tools::{self, project_root_of, Host, PermReq, Permitter, Question, SubagentRequest, SubagentResult, Todo, Tool, ToolCtx, ToolError, ToolOutput};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
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
    /// After each step: this step's tokens and cost, session total, and how full the context is.
    StepUsage { usage: Usage, cost: Option<f64>, session_cost: f64, context_tokens: u64, context_limit: Option<u64> },
    Notice(&'a str),
    /// Progress that is not an error (compaction started or finished).
    Status(&'a str),
    /// The session's todo list was replaced.
    Todos(&'a [Todo]),
    /// An event from a subagent session; nests when subagents start subagents.
    Nested { agent: &'a str, inner: &'a UiEvent<'a> },
}

/// Implemented by the front end. `ask` may be called from several tool threads; implementations
/// should serialize prompts (the engine already does).
pub trait Ui: Sync {
    fn event(&self, e: &UiEvent);
    fn ask(&self, p: &PermissionPrompt) -> Reply;
    /// Show a form for the `question` tool. `None` means the user dismissed it (which stops the run).
    fn ask_questions(&self, _questions: &[Question]) -> Option<Vec<Vec<String>>> {
        None
    }
}

/// Wraps the parent's UI for a subagent session: events are tagged, prompts go to the same user.
struct ChildUi<'a> {
    parent: &'a dyn Ui,
    agent: &'a str,
}

impl Ui for ChildUi<'_> {
    fn event(&self, e: &UiEvent) {
        self.parent.event(&UiEvent::Nested { agent: self.agent, inner: e });
    }
    fn ask(&self, p: &PermissionPrompt) -> Reply {
        self.parent.ask(p)
    }
    fn ask_questions(&self, q: &[Question]) -> Option<Vec<Vec<String>>> {
        self.parent.ask_questions(q)
    }
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
    /// Default `provider/model` (an agent's own `model` overrides it).
    pub model: String,
    pub max_tokens: u32,
    pub app_dir: PathBuf,
    pub agents: Vec<Agent>,
    /// Extra instruction files from config.
    pub instructions: Vec<String>,
    pub catalog: catalog::Catalog,
    pub compaction: compaction::Settings,
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
            agents: agent::builtin(),
            instructions: vec![],
            catalog: catalog::Catalog::embedded(),
            compaction: compaction::Settings::default(),
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
    providers: HashMap<String, Box<dyn Provider>>,
    /// Used when a model's provider has no client of its own (tests, single-provider setups).
    fallback: Option<Box<dyn Provider>>,
    cfg: EngineConfig,
    tools: Vec<Box<dyn Tool>>,
    /// Sessions whose automatic compaction failed during this process; they are not retried every step.
    auto_failed: Mutex<HashSet<String>>,
    /// One user prompt at a time across the whole process (parent and subagent sessions).
    prompt_lock: Mutex<()>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CompactResult {
    /// Token estimates before and after.
    Compacted { before: u64, after: u64 },
    NothingToCompact,
    Failed(String),
    Interrupted,
}

struct Prepared<'a> {
    session: Session,
    agent: Agent,
    req: Request,
    provider: &'a dyn Provider,
    provider_id: String,
    model_id: String,
    /// History after the latest compaction.
    eff: Vec<crate::store::Message>,
    limit: Option<u64>,
}

fn subagent_list(cfg: &EngineConfig) -> Vec<(String, String)> {
    cfg.agents.iter().filter(|a| a.subagent()).map(|a| (a.id.clone(), a.description.clone())).collect()
}

/// Add text to the trailing user message, or start one (consecutive user content merges).
fn append_user_text(messages: &mut Vec<ChatMessage>, text: String) {
    match messages.last_mut().filter(|m| m.role == Role::User) {
        Some(m) => m.content.push(Block::Text(text)),
        None => messages.push(ChatMessage { role: Role::User, content: vec![Block::Text(text)] }),
    }
}

fn add_usage(a: Usage, b: Usage) -> Usage {
    Usage { input: a.input + b.input, output: a.output + b.output, cache_read: a.cache_read + b.cache_read, cache_write: a.cache_write + b.cache_write }
}

fn human_tokens(n: u64) -> String {
    if n >= 10_000 { format!("{}k", n / 1000) } else { n.to_string() }
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
    /// Single provider answering every model ref.
    pub fn new(store: Store, provider: Box<dyn Provider>, cfg: EngineConfig) -> Self {
        Engine { store: Mutex::new(store), providers: HashMap::new(), fallback: Some(provider), tools: tools::all(subagent_list(&cfg)), cfg, auto_failed: Mutex::new(HashSet::new()), prompt_lock: Mutex::new(()) }
    }

    /// One client per provider id; a model ref's provider part picks the client.
    pub fn with_providers(store: Store, providers: HashMap<String, Box<dyn Provider>>, cfg: EngineConfig) -> Self {
        Engine { store: Mutex::new(store), providers, fallback: None, tools: tools::all(subagent_list(&cfg)), cfg, auto_failed: Mutex::new(HashSet::new()), prompt_lock: Mutex::new(()) }
    }

    pub fn catalog(&self) -> &catalog::Catalog {
        &self.cfg.catalog
    }

    pub fn connected_providers(&self) -> Vec<String> {
        let mut v: Vec<String> = self.providers.keys().cloned().collect();
        v.sort();
        v
    }

    /// Model used for a session: its own choice, else the agent's, else the configured default.
    pub fn model_ref_for(&self, session: &Session) -> String {
        let agent = agent::find(&self.cfg.agents, &session.agent);
        session.model.clone().or(agent.and_then(|a| a.model)).unwrap_or_else(|| self.cfg.model.clone())
    }

    pub fn agents(&self) -> &[Agent] {
        &self.cfg.agents
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
        self.auto_failed.lock().unwrap().remove(session_id);
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
        self.run_step_inner(sid, step, ui, cancel, false)
    }

    fn resolve_model(&self, session: &Session, agent: &Agent) -> Result<(&dyn Provider, String, String), String> {
        let model_ref = session.model.clone().or(agent.model.clone()).unwrap_or_else(|| self.cfg.model.clone());
        let (provider_id, model_id) = config::split_model(&model_ref);
        match self.providers.get(provider_id).map(|p| p.as_ref()).or(self.fallback.as_deref()) {
            Some(p) => Ok((p, provider_id.to_string(), model_id.to_string())),
            None => {
                let connected = self.connected_providers();
                Err(format!(
                    "provider '{provider_id}' is not connected (no API key). Connected: {}. Run `moco auth {provider_id}` or set {}.",
                    if connected.is_empty() { "none".to_string() } else { connected.join(", ") },
                    crate::auth::env_var_for(provider_id)
                ))
            }
        }
    }

    /// Build the request for the next step from the stored history (after the latest compaction).
    fn prepare(&self, sid: &str, step: u32, ui: &dyn Ui) -> Result<std::result::Result<Prepared<'_>, StepOutcome>, StoreError> {
        let (session, history) = {
            let st = self.store();
            (st.session(sid)?.expect("session exists"), st.messages(sid)?)
        };
        let agent = agent::find(&self.cfg.agents, &session.agent)
            .or_else(|| self.cfg.agents.first().cloned())
            .expect("at least one agent is configured");
        let advertised: Vec<&dyn Tool> = self.tools.iter().map(|t| t.as_ref()).filter(|t| agent::advertised(&agent, *t)).collect();

        let eff = compaction::effective(&history);
        let mut messages = chat::to_chat(&eff);
        let mut reminders = vec![];
        let max_steps_hit = agent.steps.is_some_and(|n| step >= n);
        if max_steps_hit {
            reminders.push(agent::MAX_STEPS.to_string());
        }
        for r in reminders {
            // Appended at request time (never stored). Only used on the last allowed step, so it never
            // changes the prefix of earlier requests.
            append_user_text(&mut messages, r);
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let lazy: Vec<PathBuf> = self.store().instruction_paths(sid)?.into_iter().map(PathBuf::from).collect();
        let ins = instructions::render(&instructions::load(&self.cfg.app_dir, std::path::Path::new(&session.directory), &self.cfg.instructions, &lazy));
        let (provider, provider_id, model_id) = match self.resolve_model(&session, &agent) {
            Ok(r) => r,
            Err(msg) => {
                ui.event(&UiEvent::Notice(&msg));
                return Ok(Err(StepOutcome::Stop(RunOutcome::Failed(msg))));
            }
        };
        let mut system = agent::system_prompt(
            &agent,
            &advertised,
            &agent::EnvInfo { cwd: &session.directory, platform: std::env::consts::OS, date: &agent::date_from_unix(now), instructions: &ins },
        );
        if agent.id == "plan" {
            // In the system prompt (not appended to the last message) so every request keeps the same prefix.
            system.push_str("\n");
            system.push_str(agent::PLAN_REMINDER);
            system.push('\n');
        }
        let req = Request {
            model: model_id.clone(),
            system,
            messages,
            tools: advertised.iter().map(|t| ToolDef { name: t.name().into(), description: t.description(), schema: t.schema() }).collect(),
            max_tokens: self.cfg.max_tokens,
            tools_disabled: max_steps_hit,
        };
        let limit = self.cfg.catalog.model(&provider_id, &model_id).map(|m| m.limit.context).filter(|l| *l > 0);
        Ok(Ok(Prepared { session, agent, req, provider, provider_id, model_id, eff, limit }))
    }

    fn run_step_inner(&self, sid: &str, step: u32, ui: &dyn Ui, cancel: &AtomicBool, overflowed: bool) -> Result<StepOutcome, StoreError> {
        let mut p = match self.prepare(sid, step, ui)? {
            Ok(p) => p,
            Err(outcome) => return Ok(outcome),
        };

        // Auto compaction: the prompt is close to the model's limit.
        if let (true, Some(limit)) = (self.cfg.compaction.auto, p.limit) {
            let size = compaction::context_size(&p.eff, compaction::estimate_request(&p.req));
            if size > self.cfg.compaction.threshold(limit) && !self.auto_failed.lock().unwrap().contains(sid) {
                match self.compact_inner(sid, "auto", ui, cancel)? {
                    CompactResult::Compacted { .. } => {
                        p = match self.prepare(sid, step, ui)? {
                            Ok(p) => p,
                            Err(outcome) => return Ok(outcome),
                        }
                    }
                    CompactResult::Interrupted => return Ok(StepOutcome::Stop(RunOutcome::Interrupted)),
                    CompactResult::Failed(e) => {
                        // Don't retry a failing summarization on every step of this run.
                        self.auto_failed.lock().unwrap().insert(sid.to_string());
                        ui.event(&UiEvent::Notice(&format!("automatic compaction failed: {e}")));
                    }
                    CompactResult::NothingToCompact => {}
                }
            }
        }
        let Prepared { session, agent, req, provider, provider_id, model_id, .. } = p;
        let (provider_id, model_id) = (provider_id.as_str(), model_id.as_str());

        let msg_id = self.store().add_message(sid, "assistant", &json!({ "parts": [] }))?;
        let declined = AtomicBool::new(false);
        let permitter = EnginePermitter {
            rules: agent.permissions.clone(),
            project_id: session.project_id.clone(),
            store: &self.store,
            ui,
            declined: &declined,
            ask_lock: &self.prompt_lock,
        };

        let mut retries = 0u32;
        let result = loop {
            let r = self.attempt(provider, &session, &req, &permitter, ui, cancel, &msg_id);
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

        // The provider rejected the prompt as too long before producing anything: compact once and retry the step.
        if let (Err(ProviderError::ContextOverflow(_)), false, false) = (&result.stream, result.dispatched, overflowed) {
            if !cancel.load(Ordering::Relaxed) {
                self.store().delete_message(&msg_id)?;
                match self.compact_inner(sid, "overflow", ui, cancel)? {
                    CompactResult::Compacted { .. } => return self.run_step_inner(sid, step, ui, cancel, true),
                    CompactResult::Interrupted => return Ok(StepOutcome::Stop(RunOutcome::Interrupted)),
                    other => {
                        let why = match other {
                            CompactResult::Failed(e) => e,
                            _ => "there is nothing older to summarize".to_string(),
                        };
                        ui.event(&UiEvent::Notice(&format!("the prompt exceeds the model's context window and could not be compacted: {why}")));
                        return Ok(StepOutcome::Stop(RunOutcome::Failed("context window exceeded".into())));
                    }
                }
            }
        }

        let tool_calls = result.parts.iter().filter(|p| p["type"] == "tool").count();
        let interrupted = cancel.load(Ordering::Relaxed) || matches!(result.stream, Err(ProviderError::Interrupted));
        let mut data = json!({ "parts": result.parts });
        if let Some((reason, usage)) = &result.finish {
            data["finish"] = json!(format!("{reason:?}"));
            data["usage"] = json!({"input": usage.input, "output": usage.output, "cacheRead": usage.cache_read, "cacheWrite": usage.cache_write});
            let model = self.cfg.catalog.model(provider_id, model_id);
            let cost = model.and_then(|m| catalog::cost(m, usage));
            let context_tokens = usage.input + usage.cache_read + usage.cache_write + usage.output;
            let session_cost = {
                let st = self.store();
                let _ = st.add_usage(sid, cost.unwrap_or(0.0), usage);
                st.usage_totals(sid).map(|t| t.0).unwrap_or(0.0)
            };
            data["cost"] = json!(cost);
            ui.event(&UiEvent::StepUsage { usage: *usage, cost, session_cost, context_tokens, context_limit: model.map(|m| m.limit.context).filter(|l| *l > 0) });
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

    /// Manual compaction (`/compact`). Holds the session claim while it runs.
    pub fn compact(&self, sid: &str, ui: &dyn Ui, cancel: &AtomicBool) -> Result<CompactResult, StoreError> {
        if !self.store().claim(sid)? {
            return Ok(CompactResult::Failed("session is busy".into()));
        }
        let r = self.compact_inner(sid, "manual", ui, cancel);
        self.store().release(sid)?;
        r
    }

    /// Summarize everything before a cut point into a checkpoint message (see `compaction.rs`).
    fn compact_inner(&self, sid: &str, trigger: &str, ui: &dyn Ui, cancel: &AtomicBool) -> Result<CompactResult, StoreError> {
        let (session, history) = {
            let st = self.store();
            (st.session(sid)?.expect("session exists"), st.messages(sid)?)
        };
        let agent = agent::find(&self.cfg.agents, &session.agent).or_else(|| self.cfg.agents.first().cloned()).expect("an agent");
        let (provider, provider_id, model_id) = match self.resolve_model(&session, &agent) {
            Ok(r) => r,
            Err(e) => return Ok(CompactResult::Failed(e)),
        };
        let eff = compaction::effective(&history);
        let Some(k) = compaction::choose_cut(&eff, self.cfg.compaction.keep) else { return Ok(CompactResult::NothingToCompact) };
        let keep_from = eff[k].id.clone();
        let update = compaction::has_checkpoint(&eff);
        let before: u64 = eff.iter().map(compaction::estimate).sum();
        let limit = self.cfg.catalog.model(&provider_id, &model_id).map(|m| m.limit.context).filter(|l| *l > 0);
        ui.event(&UiEvent::Status(&format!("compacting {k} message(s) ({trigger})...")));

        let mut portion: Vec<crate::store::Message> = eff[..k].to_vec();
        let first_droppable = usize::from(update);
        // Stay inside the model's window while summarizing: drop the oldest material first.
        if let Some(l) = limit {
            while portion.len() > first_droppable + 1 && portion.iter().map(compaction::estimate).sum::<u64>() > l * 8 / 10 {
                portion.remove(first_droppable);
            }
        }
        let mut reply = String::new();
        let mut usage_total = Usage::default();
        let mut failure = None;
        'attempts: for _ in 0..4 {
            let mut messages = chat::to_chat(&portion);
            append_user_text(&mut messages, compaction::prompt(update));
            let req = Request { model: model_id.clone(), system: compaction::SYSTEM.into(), messages, tools: vec![], max_tokens: self.cfg.max_tokens, tools_disabled: false };
            match self.summarize(provider, &req, ui, cancel) {
                Ok((text, u)) => {
                    usage_total = add_usage(usage_total, u);
                    reply = text;
                    if !compaction::follows_template(&reply) {
                        // One nudge when the reply ignores the template.
                        let mut again = req.clone();
                        again.messages.push(ChatMessage { role: Role::Assistant, content: vec![Block::Text(reply.clone())] });
                        again.messages.push(ChatMessage { role: Role::User, content: vec![Block::Text(compaction::NUDGE.into())] });
                        match self.summarize(provider, &again, ui, cancel) {
                            Ok((text2, u2)) => {
                                usage_total = add_usage(usage_total, u2);
                                if !text2.trim().is_empty() {
                                    reply = text2;
                                }
                            }
                            Err(ProviderError::Interrupted) => return Ok(CompactResult::Interrupted),
                            Err(_) => {} // keep the first reply
                        }
                    }
                    failure = None;
                    break 'attempts;
                }
                Err(ProviderError::Interrupted) => return Ok(CompactResult::Interrupted),
                Err(ProviderError::ContextOverflow(m)) if portion.len() > first_droppable + 1 => {
                    // Still too big: drop the oldest quarter and try again.
                    let n = (portion.len() - first_droppable).div_ceil(4).min(portion.len() - first_droppable - 1);
                    portion.drain(first_droppable..first_droppable + n);
                    failure = Some(m);
                }
                Err(e) => {
                    failure = Some(e.message());
                    break 'attempts;
                }
            }
        }
        if let Some(e) = failure {
            return Ok(CompactResult::Failed(e));
        }
        let summary = reply.trim().to_string();
        if summary.is_empty() {
            return Ok(CompactResult::Failed("the model returned an empty summary".into()));
        }
        let after: u64 = compaction::estimate_text(&summary) + eff[k..].iter().map(compaction::estimate).sum::<u64>();
        {
            let st = self.store();
            st.add_message(sid, "compaction", &json!({ "summary": summary, "keepFrom": keep_from, "trigger": trigger, "replaced": k }))?;
            let cost = self.cfg.catalog.model(&provider_id, &model_id).and_then(|m| catalog::cost(m, &usage_total)).unwrap_or(0.0);
            st.add_usage(sid, cost, &usage_total)?;
        }
        ui.event(&UiEvent::Status(&format!("compacted: about {} -> {} tokens", human_tokens(before), human_tokens(after))));
        Ok(CompactResult::Compacted { before, after })
    }

    /// One non-streaming-looking call: collect the text of a tool-less response, with the usual retry policy.
    fn summarize(&self, provider: &dyn Provider, req: &Request, ui: &dyn Ui, cancel: &AtomicBool) -> Result<(String, Usage), ProviderError> {
        let mut attempt = 0u32;
        loop {
            let (mut text, mut usage) = (String::new(), Usage::default());
            let res = provider.stream(req, cancel, &mut |ev| match ev {
                Event::TextDelta(t) => text.push_str(&t),
                Event::Finish { usage: u, .. } => usage = u,
                _ => {}
            });
            match res {
                Ok(()) => return Ok((text, usage)),
                Err(e) => {
                    attempt += 1;
                    match retry::delay(attempt, &e, (self.cfg.jitter)()) {
                        Some(wait) if !cancel.load(Ordering::Relaxed) => {
                            ui.event(&UiEvent::Retry { attempt, wait, error: &e.message() });
                            (self.cfg.sleep)(wait, cancel);
                            if cancel.load(Ordering::Relaxed) {
                                return Err(ProviderError::Interrupted);
                            }
                        }
                        _ => return Err(e),
                    }
                }
            }
        }
    }

    /// One streaming request. Tools start executing as soon as their call is complete.
    fn attempt(&self, provider: &dyn Provider, session: &Session, req: &Request, permitter: &EnginePermitter, ui: &dyn Ui, cancel: &AtomicBool, msg_id: &str) -> AttemptResult {
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
            let res = provider.stream(req, cancel, &mut |ev| match ev {
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
                            Some(t) => {
                                let host = EngineHost { engine: self, session, ui, cancel };
                                t.run(&input, &ToolCtx { session_id: &session.id, host: &host, cwd, project_root: root, spill_dir: spill, permitter, cancel })
                            }
                        }),
                    ));
                }
                Event::Finish { reason, usage } => finish = Some((reason, usage)),
            });
            for (idx, h) in handles {
                let outcome = h.join().unwrap_or_else(|_| Err(ToolError::fail("tool panicked")));
                declined |= outcome == Err(ToolError::Declined);
                let name = parts[idx]["name"].as_str().unwrap_or("").to_string();
                if let (true, Ok(out)) = (name == "read", &outcome) {
                    // Reading a file pulls in AGENTS.md files between it and the working directory.
                    if let Some(p) = out.metadata["path"].as_str() {
                        let st = self.store();
                        for ins in instructions::discover_between(std::path::Path::new(p), &cwd) {
                            let _ = st.add_instruction(&session.id, &ins.to_string_lossy());
                        }
                    }
                }
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

/// The engine services a tool can use: todos, questions and subagents of the calling session.
struct EngineHost<'a> {
    engine: &'a Engine,
    session: &'a Session,
    ui: &'a dyn Ui,
    cancel: &'a AtomicBool,
}

const MAX_SUBAGENT_DEPTH: usize = 3;

impl Host for EngineHost<'_> {
    fn set_todos(&self, todos: Vec<Todo>) -> Result<(), ToolError> {
        self.engine.store().set_todos(&self.session.id, &todos).map_err(|e| ToolError::fail(e.to_string()))?;
        self.ui.event(&UiEvent::Todos(&todos));
        Ok(())
    }

    fn ask(&self, questions: &[Question]) -> Result<Vec<Vec<String>>, ToolError> {
        let _one_prompt_at_a_time = self.engine.prompt_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.ui.ask_questions(questions).ok_or(ToolError::Declined)
    }

    fn subagent(&self, req: SubagentRequest) -> Result<SubagentResult, ToolError> {
        let eng = self.engine;
        let store_err = |e: StoreError| ToolError::fail(e.to_string());
        let agent = agent::find(&eng.cfg.agents, &req.agent).filter(|a| a.subagent()).ok_or_else(|| ToolError::fail(format!("Unknown agent '{}'", req.agent)))?;
        let child = {
            let st = eng.store();
            if st.depth(&self.session.id).map_err(store_err)? + 1 > MAX_SUBAGENT_DEPTH {
                return Err(ToolError::fail("subagent depth limit reached; do this work yourself"));
            }
            let child = match &req.session_id {
                Some(id) => st
                    .session(id)
                    .map_err(store_err)?
                    .filter(|c| c.parent_id.as_deref() == Some(self.session.id.as_str()))
                    .ok_or_else(|| ToolError::fail(format!("sessionID {id} is not a subagent session of this conversation")))?,
                None => st.create_child_session(self.session, &agent.id).map_err(store_err)?,
            };
            // A model chosen for this conversation carries over unless the call names one.
            match req.model.as_ref().or(self.session.model.as_ref()) {
                Some(m) => st.set_model(&child.id, Some(m)).map_err(store_err)?,
                None => {}
            }
            child
        };
        eng.submit(&child.id, &req.prompt, Mode::Steer).map_err(store_err)?;
        let child_ui = ChildUi { parent: self.ui, agent: &agent.id };
        self.ui.event(&UiEvent::Status(&format!("subagent {}: {}", agent.id, req.description)));
        match eng.run(&child.id, &child_ui, self.cancel, false).map_err(store_err)? {
            RunOutcome::Idle => {}
            RunOutcome::Interrupted => return Err(ToolError::Interrupted),
            RunOutcome::Declined => return Err(ToolError::Declined),
            RunOutcome::Failed(m) => return Err(ToolError::fail(format!("Subagent failed: {m}"))),
            RunOutcome::Busy => return Err(ToolError::fail("that subagent session is already running")),
        }
        let output = eng
            .store()
            .messages(&child.id)
            .map_err(store_err)?
            .iter()
            .rev()
            .filter(|m| m.kind == "assistant")
            .find_map(|m| {
                let text: Vec<&str> = m.data["parts"].as_array()?.iter().filter(|p| p["type"] == "text").filter_map(|p| p["text"].as_str()).collect();
                (!text.is_empty()).then(|| text.join("\n\n"))
            })
            .unwrap_or_else(|| "(the subagent returned no text)".to_string());
        Ok(SubagentResult { session_id: child.id, output })
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
    ask_lock: &'a Mutex<()>,
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
        questions: Mutex<Vec<Vec<Question>>>,
        answers: Mutex<Option<Vec<Vec<String>>>>,
    }
    impl TestUi {
        fn replying(r: Vec<Reply>) -> Self {
            TestUi { replies: Mutex::new(r.into()), ..Default::default() }
        }
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }
    fn describe(e: &UiEvent) -> String {
        match e {
            UiEvent::TextDelta(t) => format!("text:{t}"),
            UiEvent::ReasoningDelta(_) => "reasoning".into(),
            UiEvent::ToolStarted { name, .. } => format!("start:{name}"),
            UiEvent::ToolFinished { name, ok, .. } => format!("done:{name}:{ok}"),
            UiEvent::Retry { attempt, wait, .. } => format!("retry:{attempt}:{}", wait.as_secs()),
            UiEvent::StepUsage { .. } => "usage".into(),
            UiEvent::Notice(n) => format!("notice:{n}"),
            UiEvent::Status(n) => format!("status:{n}"),
            UiEvent::Todos(t) => format!("todos:{}", t.len()),
            UiEvent::Nested { agent, inner } => format!("{agent}>{}", describe(inner)),
        }
    }

    impl Ui for TestUi {
        fn event(&self, e: &UiEvent) {
            let line = describe(e);
            if let Some(h) = &*self.hook.lock().unwrap() {
                h(&line);
            }
            self.log.lock().unwrap().push(line);
        }
        fn ask_questions(&self, q: &[Question]) -> Option<Vec<Vec<String>>> {
            self.questions.lock().unwrap().push(q.to_vec());
            self.answers.lock().unwrap().clone()
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
        fixture_cfg(script, |_| {})
    }

    fn fixture_cfg(script: Vec<Scripted>, tweak: impl FnOnce(&mut EngineConfig)) -> Fixture {
        let dir = std::env::temp_dir().join(crate::id::new_id("moco-eng"));
        std::fs::create_dir_all(dir.join("work")).unwrap();
        let fake = Arc::new(FakeProvider::new(script));
        let waits = Arc::new(Mutex::new(vec![]));
        let mut cfg = EngineConfig::new("test-model", dir.join("app"));
        let w = waits.clone();
        cfg.sleep = Box::new(move |d, _| w.lock().unwrap().push(d));
        cfg.jitter = Box::new(|| 0.0);
        tweak(&mut cfg);
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
        assert_eq!(req.tools.len(), 11);
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
        assert!(reqs[0].system.contains("You are in Plan mode") && reqs[1].system.contains("You are in Plan mode"));
        assert!(!all_text(&reqs[0]).contains("Plan mode"), "the reminder lives in the system prompt, not the messages");
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

    #[test]
    fn instructions_reach_the_system_prompt_and_lazy_files_stick() {
        let f = fixture(vec![tool_call("t1", "read", json!({"path":"pkg/a.txt"})), text("ok"), text("again")]);
        f.file("AGENTS.md", "ROOT-RULE");
        std::fs::create_dir_all(f.dir.join("work/pkg")).unwrap();
        f.file("pkg/AGENTS.md", "PKG-RULE");
        f.file("pkg/a.txt", "x");
        f.say("read it");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        f.say("and again");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert!(reqs[0].system.contains("ROOT-RULE") && !reqs[0].system.contains("PKG-RULE"));
        assert!(reqs[1].system.contains("PKG-RULE"), "discovered by the read, included from the next step on");
        assert!(reqs[2].system.contains("PKG-RULE"), "and kept for later turns");
        assert_eq!(f.engine.store().instruction_paths(&f.session.id).unwrap().len(), 1);
    }

    #[test]
    fn config_rules_make_shell_ask_and_agent_model_overrides() {
        let dir = std::env::temp_dir().join(crate::id::new_id("moco-eng-cfg"));
        std::fs::create_dir_all(dir.join("work")).unwrap();
        let cfg_json: config::Config = serde_json::from_value(json!({
            "permissions": [{"action":"shell","resource":"*","effect":"ask"}],
            "agents": {"build": {"model": "anthropic/claude-other"}}
        }))
        .unwrap();
        let fake = Arc::new(FakeProvider::new(vec![tool_call("t1", "shell", json!({"command":"touch ran"})), text("done")]));
        let mut cfg = EngineConfig::new("anthropic/claude-default", dir.join("app"));
        cfg.agents = config::resolve_agents(&cfg_json);
        let store = Store::open_memory().unwrap();
        let ses = store.create_session(dir.join("work").to_str().unwrap(), "build").unwrap();
        let engine = Engine::new(store, Box::new(Shared(fake.clone())), cfg);
        engine.submit(&ses.id, "go", Mode::Steer).unwrap();
        let ui = TestUi::replying(vec![Reply::Reject(None)]);
        assert_eq!(engine.run(&ses.id, &ui, &AtomicBool::new(false), false).unwrap(), RunOutcome::Declined);
        assert_eq!(ui.asked.lock().unwrap().as_slice(), ["shell touch ran"]);
        assert!(!dir.join("work/ran").exists());
        assert_eq!(fake.requests.lock().unwrap()[0].model, "claude-other");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn model_ref_routes_to_the_right_provider_and_reports_cost() {
        let dir = std::env::temp_dir().join(crate::id::new_id("moco-eng-route"));
        std::fs::create_dir_all(dir.join("work")).unwrap();
        let a = Arc::new(FakeProvider::new(vec![text("from a")]));
        let b = Arc::new(FakeProvider::new(vec![text("from b")]));
        let providers: HashMap<String, Box<dyn Provider>> =
            HashMap::from([("anthropic".to_string(), Box::new(Shared(a.clone())) as Box<dyn Provider>), ("deepseek".to_string(), Box::new(Shared(b.clone())))]);
        let store = Store::open_memory().unwrap();
        let ses = store.create_session(dir.join("work").to_str().unwrap(), "build").unwrap();
        let engine = Engine::with_providers(store, providers, EngineConfig::new("anthropic/claude-sonnet-5-5", dir.join("app")));
        let usage_events = Mutex::new(vec![]);
        struct Capture<'a>(&'a Mutex<Vec<(Option<f64>, f64, u64, Option<u64>)>>);
        impl Ui for Capture<'_> {
            fn event(&self, e: &UiEvent) {
                if let UiEvent::StepUsage { cost, session_cost, context_tokens, context_limit, .. } = e {
                    self.0.lock().unwrap().push((*cost, *session_cost, *context_tokens, *context_limit));
                }
            }
            fn ask(&self, _: &PermissionPrompt) -> Reply {
                Reply::Reject(None)
            }
        }
        let ui = Capture(&usage_events);
        engine.submit(&ses.id, "one", Mode::Steer).unwrap();
        engine.run(&ses.id, &ui, &AtomicBool::new(false), false).unwrap();
        engine.store().set_model(&ses.id, Some("deepseek/deepseek-chat")).unwrap();
        engine.submit(&ses.id, "two", Mode::Steer).unwrap();
        engine.run(&ses.id, &ui, &AtomicBool::new(false), false).unwrap();
        assert_eq!((a.calls(), b.calls()), (1, 1));
        assert_eq!(b.requests.lock().unwrap()[0].model, "deepseek-chat");
        let ev = usage_events.lock().unwrap().clone();
        // 10 input + 5 output tokens at $2 / $10 per million.
        assert!((ev[0].0.unwrap() - 0.00007).abs() < 1e-12, "{ev:?}");
        assert_eq!((ev[0].2, ev[0].3), (15, Some(1_000_000)));
        assert!(ev[1].1 >= ev[0].1, "session cost accumulates");
        let (total, tokens) = engine.store().usage_totals(&ses.id).unwrap();
        assert!(total >= ev[0].1 && tokens.input == 20 && tokens.output == 10, "{total} {tokens:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unconnected_provider_gives_an_actionable_error_before_any_request() {
        let dir = std::env::temp_dir().join(crate::id::new_id("moco-eng-noprov"));
        std::fs::create_dir_all(dir.join("work")).unwrap();
        let store = Store::open_memory().unwrap();
        let ses = store.create_session(dir.join("work").to_str().unwrap(), "build").unwrap();
        let engine = Engine::with_providers(store, HashMap::new(), EngineConfig::new("openai/gpt-x", dir.join("app")));
        engine.submit(&ses.id, "hi", Mode::Steer).unwrap();
        let ui = TestUi::default();
        match engine.run(&ses.id, &ui, &AtomicBool::new(false), false).unwrap() {
            RunOutcome::Failed(m) => assert!(m.contains("'openai' is not connected") && m.contains("moco auth openai") && m.contains("OPENAI_API_KEY"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(engine.store().messages(&ses.id).unwrap().len(), 1, "only the user message; no empty assistant message");
        let _ = std::fs::remove_dir_all(dir);
    }

    const SUMMARY: &str = "## Objective\nSUMMARY-BODY\n## Work State\n### Completed\n- things\n## Next Move\n1. go on";

    /// Catalog entry that gives `anthropic/claude-test` a small context window.
    fn small_window(cfg: &mut EngineConfig, limit: u64, keep: u64) {
        cfg.model = "anthropic/claude-test".into();
        cfg.catalog.apply_config(&std::collections::BTreeMap::from([(
            "anthropic".to_string(),
            config::ProviderConfig {
                models: std::collections::BTreeMap::from([(
                    "claude-test".to_string(),
                    config::ModelConfig { limit: Some(catalog::Limit { context: limit, output: 1000 }), ..Default::default() },
                )]),
                ..Default::default()
            },
        )]));
        cfg.compaction = compaction::Settings { auto: true, buffer: compaction::Buffer::Tokens(500), keep };
    }

    /// `n` alternating user/assistant messages of about 1000 tokens each.
    fn seed(f: &Fixture, n: usize) -> Vec<String> {
        let st = f.engine.store();
        (0..n)
            .map(|i| {
                let pad = format!("{} {}", if i % 2 == 0 { "OLD-USER" } else { "OLD-ASSISTANT" }, "x".repeat(3900));
                if i % 2 == 0 {
                    st.add_message(&f.session.id, "user", &json!({"text": pad})).unwrap()
                } else {
                    st.add_message(&f.session.id, "assistant", &json!({"parts":[{"type":"text","text":pad}]})).unwrap()
                }
            })
            .collect()
    }

    fn all_text(req: &Request) -> String {
        req.messages.iter().flat_map(|m| &m.content).filter_map(|b| if let Block::Text(t) = b { Some(t.as_str()) } else { None }).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn auto_compaction_summarizes_the_past_and_keeps_the_tail() {
        let f = fixture_cfg(vec![text(SUMMARY), text("the answer")], |c| small_window(c, 3000, 1000));
        seed(&f, 4);
        f.say("next question");
        let ui = TestUi::default();
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        // 1: the summarization call: no tools, old history, the create prompt last.
        assert!(reqs[0].tools.is_empty() && all_text(&reqs[0]).contains("OLD-USER") && all_text(&reqs[0]).contains("You MUST summarize"));
        assert!(!all_text(&reqs[0]).contains("next question"), "the tail is not summarized");
        // 2: the real step: checkpoint + tail only, same system prompt as always, tools back.
        assert_eq!(reqs[1].tools.len(), 11);
        let t = all_text(&reqs[1]);
        assert!(t.contains(compaction::SUMMARY_PREFIX) && t.contains("SUMMARY-BODY") && t.contains("next question"), "{t}");
        assert!(!t.contains("OLD-USER") && !t.contains("OLD-ASSISTANT"));
        assert_eq!(reqs[1].messages.len(), 1, "checkpoint and the new prompt merge into one user message");
        let stored = f.msgs();
        let c = stored.iter().find(|m| m.kind == "compaction").expect("compaction message");
        let next = stored.iter().find(|m| m.kind == "user" && m.data["text"] == "next question").unwrap();
        assert_eq!((c.data["keepFrom"].as_str(), c.data["trigger"].as_str(), c.data["replaced"].as_u64()), (Some(next.id.as_str()), Some("auto"), Some(4)));
        assert!(ui.log().iter().any(|l| l.starts_with("status:compacting 4 message(s) (auto)")) && ui.log().iter().any(|l| l.starts_with("status:compacted: about")));
        // Nothing is deleted from the store; only the model's view changes.
        assert_eq!(stored.iter().filter(|m| m.kind == "user" || m.kind == "assistant").count(), 6);
    }

    #[test]
    fn a_reply_that_ignores_the_template_gets_one_nudge() {
        let f = fixture_cfg(vec![text("Sure! Here is what we did."), text(SUMMARY), text("done")], |c| small_window(c, 3000, 1000));
        seed(&f, 4);
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert_eq!(reqs.len(), 3);
        let nudge = reqs[1].messages.last().unwrap();
        assert!(matches!(&nudge.content[0], Block::Text(t) if t == compaction::NUDGE));
        assert!(matches!(&reqs[1].messages[reqs[1].messages.len() - 2].content[0], Block::Text(t) if t == "Sure! Here is what we did."));
        assert!(all_text(&reqs[2]).contains("SUMMARY-BODY"), "the conforming reply was kept");
    }

    #[test]
    fn overflow_triggers_one_compaction_and_a_retry_but_not_a_loop() {
        let overflow = || Scripted::Fail(vec![], ProviderError::ContextOverflow("prompt is too long".into()));
        // Recovers: overflow -> summary -> answer.
        let f = fixture_cfg(vec![overflow(), text(SUMMARY), text("ok now")], |c| c.compaction.keep = 1000);
        seed(&f, 4);
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        assert_eq!(f.fake.calls(), 3);
        let stored = f.msgs();
        assert_eq!(stored.iter().find(|m| m.kind == "compaction").unwrap().data["trigger"], "overflow");
        assert!(stored.iter().filter(|m| m.kind == "assistant").all(|m| !m.data["parts"].as_array().unwrap().is_empty()), "no empty assistant message left behind");
        // Overflows again right after compacting: surfaced, not retried forever.
        let f = fixture_cfg(vec![overflow(), text(SUMMARY), overflow()], |c| c.compaction.keep = 1000);
        seed(&f, 4);
        f.say("go");
        assert!(matches!(f.run(&TestUi::default()), RunOutcome::Failed(_)));
        assert_eq!(f.fake.calls(), 3);
    }

    #[test]
    fn overflow_with_nothing_to_summarize_fails_clearly() {
        let f = fixture_cfg(vec![Scripted::Fail(vec![], ProviderError::ContextOverflow("too long".into()))], |_| {});
        f.say("hello");
        let ui = TestUi::default();
        assert_eq!(f.run(&ui), RunOutcome::Failed("context window exceeded".into()));
        assert_eq!(f.fake.calls(), 1);
        assert!(ui.log().iter().any(|l| l.contains("could not be compacted") && l.contains("nothing older")));
    }

    #[test]
    fn a_second_compaction_updates_the_checkpoint() {
        let f = fixture_cfg(vec![text(SUMMARY), text("## Objective\nSECOND-SUMMARY\n## Next Move\n1. x\n## Decisions\n(none)")], |c| small_window(c, 3000, 1000));
        seed(&f, 4);
        let ui = TestUi::default();
        let cancel = AtomicBool::new(false);
        assert!(matches!(f.engine.compact(&f.session.id, &ui, &cancel).unwrap(), CompactResult::Compacted { .. }));
        seed(&f, 4);
        assert!(matches!(f.engine.compact(&f.session.id, &ui, &cancel).unwrap(), CompactResult::Compacted { .. }));
        let reqs = f.fake.requests.lock().unwrap();
        let second = all_text(&reqs[1]);
        assert!(second.contains("SUMMARY-BODY"), "previous checkpoint is the input");
        assert!(second.contains("Update the existing checkpoint") && !second.contains("You MUST summarize"));
        let eff = compaction::effective(&f.msgs());
        assert!(eff[0].data["text"].as_str().unwrap().contains("SECOND-SUMMARY") && !eff[0].data["text"].as_str().unwrap().contains("SUMMARY-BODY"));
    }

    #[test]
    fn manual_compaction_edge_cases() {
        let f = fixture_cfg(vec![], |c| small_window(c, 3000, 1000));
        let ui = TestUi::default();
        let cancel = AtomicBool::new(false);
        f.say("hi");
        f.engine.store().deliver(&f.session.id, true).unwrap();
        assert_eq!(f.engine.compact(&f.session.id, &ui, &cancel).unwrap(), CompactResult::NothingToCompact);
        assert!(f.engine.store().claim(&f.session.id).unwrap());
        assert_eq!(f.engine.compact(&f.session.id, &ui, &cancel).unwrap(), CompactResult::Failed("session is busy".into()));
        assert_eq!(f.fake.calls(), 0);
    }

    #[test]
    fn failed_auto_compaction_does_not_block_the_step_or_repeat() {
        let f = fixture_cfg(
            vec![Scripted::Fail(vec![], ProviderError::Auth("no access".into())), tool_call("t1", "shell", json!({"command":"true"})), text("finished")],
            |c| small_window(c, 3000, 1000),
        );
        seed(&f, 4);
        f.say("go");
        let ui = TestUi::default();
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        assert_eq!(f.fake.calls(), 3, "one failed summarization, then two normal steps; no second summarization attempt");
        assert!(ui.log().iter().any(|l| l.starts_with("notice:automatic compaction failed")));
        assert!(f.msgs().iter().all(|m| m.kind != "compaction"));
    }

    #[test]
    fn auto_compaction_can_be_turned_off_and_needs_a_known_limit() {
        let f = fixture_cfg(vec![text("plain")], |c| {
            small_window(c, 3000, 1000);
            c.compaction.auto = false;
        });
        seed(&f, 4);
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        assert_eq!(f.fake.calls(), 1);
        let g = fixture(vec![text("plain")]); // test-model has no catalog entry: no limit, no auto compaction
        seed(&g, 40);
        g.say("go");
        assert_eq!(g.run(&TestUi::default()), RunOutcome::Idle);
        assert_eq!(g.fake.calls(), 1);
    }

    fn result_text(req: &Request) -> String {
        req.messages
            .last()
            .unwrap()
            .content
            .iter()
            .filter_map(|b| if let Block::ToolResult { content, .. } = b { Some(content.as_str()) } else { None })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn child_id(req: &Request) -> String {
        result_text(req).lines().next().unwrap().trim_start_matches("sessionID: ").to_string()
    }

    fn sub(id: &str, agent: &str, prompt: &str) -> Scripted {
        tool_call(id, "subagent", json!({"agent": agent, "description": "do it", "prompt": prompt}))
    }

    #[test]
    fn subagent_runs_in_a_child_session_and_returns_its_final_text() {
        let f = fixture(vec![sub("t1", "general", "find the thing"), text("child result"), text("parent done")]);
        f.say("delegate");
        let ui = TestUi::default();
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert_eq!(reqs.len(), 3);
        // The child sees only its prompt, with its own agent's tool set (no question, no subagent).
        assert_eq!(all_text(&reqs[1]).trim(), "find the thing");
        let names: Vec<&str> = reqs[1].tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"shell") && !names.contains(&"subagent") && !names.contains(&"question"), "{names:?}");
        // The parent gets the text plus the id to continue with.
        let out = result_text(&reqs[2]);
        assert!(out.starts_with("sessionID: ses_") && out.ends_with("child result"), "{out}");
        let cid = child_id(&reqs[2]);
        let child = f.engine.store().session(&cid).unwrap().unwrap();
        assert_eq!((child.parent_id.as_deref(), child.agent.as_str()), (Some(f.session.id.as_str()), "general"));
        assert_eq!(f.engine.store().messages(&cid).unwrap().len(), 2);
        let log = ui.log();
        assert!(log.contains(&"status:subagent general: do it".to_string()) && log.contains(&"general>text:child result".to_string()), "{log:?}");
        // The tool description lists the agents it can start.
        let desc = &reqs[0].tools.iter().find(|t| t.name == "subagent").unwrap().description;
        assert!(desc.contains("- general:") && desc.contains("- explore:") && !desc.contains("- build:"));
    }

    #[test]
    fn a_subagent_session_can_be_continued_by_id() {
        let f = fixture(vec![sub("t1", "general", "first job"), text("c1"), text("p1")]);
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let cid = child_id(&f.fake.requests.lock().unwrap()[2]);
        f.fake.extend(vec![
            tool_call("t2", "subagent", json!({"agent":"general","description":"more","prompt":"second job","sessionID":cid})),
            text("c2"),
            tool_call("t3", "subagent", json!({"agent":"general","description":"more","prompt":"x","sessionID":"ses_not_mine"})),
            text("p2"),
        ]);
        f.say("again");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        let t = all_text(&reqs[4]);
        assert!(t.contains("first job") && t.contains("c1") && t.contains("second job"), "the child kept its history: {t}");
        assert!(matches!(&reqs[6].messages.last().unwrap().content[0], Block::ToolResult { is_error: true, content, .. } if content.contains("not a subagent session")));
    }

    #[test]
    fn child_permission_prompts_go_through_the_parent_ui() {
        let cfg: config::Config = serde_json::from_value(json!({"permissions":[{"action":"shell","resource":"*","effect":"ask"}]})).unwrap();
        let script = || vec![sub("t1", "general", "run it"), tool_call("c1", "shell", json!({"command":"echo hi"})), text("ran"), text("done")];
        let f = fixture_cfg(script(), |c| c.agents = config::resolve_agents(&cfg));
        f.say("go");
        let ui = TestUi::replying(vec![Reply::Once]);
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        assert_eq!(ui.asked.lock().unwrap().as_slice(), ["shell echo hi"]);
        // Rejecting inside the child stops the whole run.
        let f = fixture_cfg(script(), |c| c.agents = config::resolve_agents(&cfg));
        f.say("go");
        let ui = TestUi::replying(vec![Reply::Reject(None)]);
        assert_eq!(f.run(&ui), RunOutcome::Declined);
        assert_eq!(f.fake.calls(), 2, "no further model calls after the rejection");
    }

    #[test]
    fn explore_is_read_only() {
        let f = fixture(vec![sub("t1", "explore", "look"), tool_call("c1", "write", json!({"path":"x.txt","content":"c"})), text("could not write"), text("done")]);
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        let names: Vec<&str> = reqs[1].tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["read", "glob", "grep", "shell", "webfetch"]);
        assert!(reqs[1].system.contains("file search specialist"));
        assert!(matches!(&reqs[2].messages.last().unwrap().content[0], Block::ToolResult { is_error: true, content, .. } if content.starts_with("Permission denied: edit")));
        assert!(!f.dir.join("work/x.txt").exists());
    }

    #[test]
    fn subagent_depth_is_limited() {
        let cfg: config::Config = serde_json::from_value(json!({"agents":{"deep":{"mode":"all","description":"recurses"}}})).unwrap();
        let mut script: Vec<Scripted> = (0..4).map(|i| sub(&format!("t{i}"), "deep", "go deeper")).collect();
        script.extend([text("c3"), text("c2"), text("c1"), text("p")]);
        let f = fixture_cfg(script, |c| c.agents = config::resolve_agents(&cfg));
        f.engine.store().set_agent(&f.session.id, "deep").unwrap();
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert_eq!(reqs.len(), 8);
        assert!(result_text(&reqs[4]).contains("depth limit"), "{}", result_text(&reqs[4]));
    }

    #[test]
    fn todowrite_and_question_reach_the_user() {
        let f = fixture(vec![
            tool_call("t1", "todowrite", json!({"todos":[{"content":"a","status":"in_progress"},{"content":"b","status":"pending"}]})),
            tool_call("t2", "question", json!({"questions":[{"question":"Which DB?","header":"DB","options":[{"label":"SQLite","description":"embedded"}]}]})),
            text("ok"),
        ]);
        f.say("plan it");
        let ui = TestUi::default();
        *ui.answers.lock().unwrap() = Some(vec![vec!["SQLite".into()]]);
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        assert_eq!(f.engine.store().todos(&f.session.id).unwrap().len(), 2);
        assert!(ui.log().contains(&"todos:2".to_string()));
        assert_eq!(ui.questions.lock().unwrap()[0][0].question, "Which DB?");
        let reqs = f.fake.requests.lock().unwrap();
        assert!(result_text(&reqs[2]).contains("\"Which DB?\"=\"SQLite\""));
        // Dismissing the form stops the run.
        let f = fixture(vec![tool_call("t2", "question", json!({"questions":[{"question":"Q?","header":"Q","options":[]}]}))]);
        f.say("ask");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Declined);
        assert_eq!(f.fake.calls(), 1);
    }

    #[test]
    fn every_request_of_a_turn_extends_the_previous_one() {
        // Prompt caching needs a stable prefix: system, tools and earlier messages must not change between steps,
        // for the plan agent too (whose reminder used to move with the last message).
        for agent in ["build", "plan"] {
            let f = fixture(vec![
                tool_call("t1", "read", json!({"path":"a.txt"})),
                tool_call("t2", "glob", json!({"pattern":"*.txt"})),
                tool_call("t3", "grep", json!({"pattern":"x"})),
                text("done"),
            ]);
            f.engine.store().set_agent(&f.session.id, agent).unwrap();
            f.file("a.txt", "x\n");
            f.say("look around");
            assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
            let reqs = f.fake.requests.lock().unwrap();
            assert_eq!(reqs.len(), 4);
            for w in reqs.windows(2) {
                assert_eq!(w[0].system, w[1].system, "{agent}: system prompt");
                assert_eq!(w[0].tools, w[1].tools, "{agent}: tools");
                assert_eq!(&w[1].messages[..w[0].messages.len()], &w[0].messages[..], "{agent}: earlier messages");
                assert!(w[1].messages.len() > w[0].messages.len());
            }
        }
    }
}
