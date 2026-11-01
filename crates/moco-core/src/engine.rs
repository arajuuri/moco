//! Session engine: the step loop (docs/03-agent-loop.md).
//!
//! `run` claims the session, then repeats: deliver inbox items allowed at this boundary, run one step
//! (one LLM call, tools executing concurrently as their calls complete), continue while the step
//! produced tool calls. History is reloaded from the store on every step.
use crate::agent::{self, Agent};
use crate::snapshot::{FileDiff, Snapshots};
use crate::{blobs, catalog, classifier, compaction, config, instructions, skills, title};
use crate::chat;
use crate::permission::{self, Effect, PermissionMode, Rule};
use crate::provider::{retry, Block, ChatMessage, Event, FinishReason, Provider, ProviderError, Request, Role, ToolDef, Usage};
use crate::store::{Mode, Session, Store, StoreError};
use crate::tools::{self, project_root_of, Host, MediaCaps, PermReq, Permitter, Question, SubagentRequest, SubagentResult, Todo, Tool, ToolCtx, ToolError, ToolOutput};
use serde_json::{json, Value};
mod classify;
mod jobs;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
    /// The permission classifier was called (`classifier.rs`): what the call cost, the session total (which includes
    /// it) and the part of that total spent on classifying.
    ClassifierUsage { cost: f64, session_cost: f64, classifier_cost: f64 },
    Notice(&'a str),
    /// Progress that is not an error (compaction started or finished).
    Status(&'a str),
    /// The session's todo list was replaced.
    Todos(&'a [Todo]),
    /// A generated title was stored for the session.
    Titled(&'a str),
    /// Prompts from the inbox became part of the conversation (at a step boundary or when the session was idle).
    Delivered,
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
    /// Nobody can answer a prompt (a background subagent). The command classifier then runs only if its
    /// `background` setting allows it.
    fn unattended(&self) -> bool {
        false
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
    fn unattended(&self) -> bool {
        self.parent.unattended()
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

/// See `Engine::effort_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortInfo {
    pub desired: Option<String>,
    /// The model offers `desired` (otherwise requests go out without it).
    pub offered: bool,
    pub levels: Vec<String>,
}

pub struct EngineConfig {
    /// Default `provider/model` (an agent's own `model` overrides it).
    pub model: String,
    pub max_tokens: u32,
    pub app_dir: PathBuf,
    /// The agents in the default permission mode (`permission_mode`); also the list for sessions without agents of their own.
    pub agents: Vec<Agent>,
    /// Default permission mode for sessions that have not chosen one.
    pub permission_mode: PermissionMode,
    /// The agent list for each mode. A session uses the list of its own mode; when a mode has no entry, `agents` is used.
    pub agents_by_mode: BTreeMap<PermissionMode, Vec<Agent>>,
    /// Extra instruction files from config.
    pub instructions: Vec<String>,
    pub catalog: catalog::Catalog,
    pub compaction: compaction::Settings,
    /// Capture the working tree before every step so changes can be undone (`/undo`) and diffed.
    pub snapshots: bool,
    pub snapshot_max_files: usize,
    /// Model for titles and other small jobs (`provider/model`); the session's model when unset.
    pub small_model: Option<String>,
    /// Automatic classification of shell commands that would otherwise ask (off unless set; see `classifier.rs`).
    pub classifier: Option<classifier::Settings>,
    /// Discovered skills (see `skills::discover`).
    pub skills: Vec<skills::Skill>,
    /// MCP servers whose tools join the built-in ones. Their tool list is read again for every request.
    pub mcp: Option<Arc<crate::mcp::Manager>>,
    /// Run on files the model writes, edits or patches (see `format.rs`).
    pub formatters: crate::format::Formatters,
    /// The `websearch` tool is offered when this is set (see `tools/websearch.rs`).
    pub websearch: Option<tools::websearch::Settings>,
    /// Default reasoning effort (config `effort`); a session's or agent's own choice wins.
    pub effort: Option<String>,
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
            permission_mode: PermissionMode::default(),
            agents_by_mode: BTreeMap::new(),
            instructions: vec![],
            catalog: catalog::Catalog::embedded(),
            compaction: compaction::Settings::default(),
            snapshots: true,
            snapshot_max_files: 50_000,
            small_model: None,
            classifier: None,
            skills: vec![],
            mcp: None,
            formatters: Default::default(),
            websearch: None,
            effort: None,
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
    /// Built-in tools; MCP tools are added per request (`current_tools`).
    tools: Vec<Arc<dyn Tool>>,
    /// Sessions whose automatic compaction failed during this process; they are not retried every step.
    auto_failed: Mutex<HashSet<String>>,
    /// One user prompt at a time across the whole process (parent and subagent sessions).
    prompt_lock: Mutex<()>,
    /// Sessions whose snapshots failed in this process (too many files, I/O error); they are skipped from then on.
    snapshot_off: Mutex<HashSet<String>>,
    /// Sessions a title was already attempted for in this process.
    title_tried: Mutex<HashSet<String>>,
    /// Commands the classifier allowed in this process (per session and directory); only allows are remembered.
    classified: Mutex<HashSet<String>>,
    /// Classifier models already reported as having no price (once each per process).
    unpriced_noted: Mutex<HashSet<String>>,
    /// Weak handle to this engine once it is shared (`share`): detached background threads need an owner.
    me: Mutex<std::sync::Weak<Engine>>,
    jobs: jobs::Jobs,
    /// Told about background job completions (the UI prints them).
    notifier: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct UndoReport {
    pub restored: Vec<String>,
    pub deleted: Vec<String>,
    /// Messages hidden by this undo.
    pub hidden_messages: usize,
    /// False when the turn has no snapshot (or snapshots are off): only the conversation was rolled back.
    pub files_restored: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UndoResult {
    Done(UndoReport),
    NothingToUndo,
    Busy,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiffResult {
    Files(Vec<FileDiff>),
    NoSnapshots,
    Failed(String),
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
    /// The tool list the request was built from (tool calls are looked up here).
    tools: Vec<Arc<dyn Tool>>,
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
        Engine { store: Mutex::new(store), providers: HashMap::new(), fallback: Some(provider), tools: tools::all(subagent_list(&cfg), cfg.skills.clone(), cfg.websearch.clone()).into_iter().map(Arc::from).collect(), cfg, auto_failed: Mutex::new(HashSet::new()), prompt_lock: Mutex::new(()), snapshot_off: Mutex::new(HashSet::new()), title_tried: Mutex::new(HashSet::new()), classified: Mutex::new(HashSet::new()), unpriced_noted: Mutex::new(HashSet::new()), me: Mutex::new(std::sync::Weak::new()), jobs: jobs::Jobs::default(), notifier: Mutex::new(None) }
    }

    /// One client per provider id; a model ref's provider part picks the client.
    pub fn with_providers(store: Store, providers: HashMap<String, Box<dyn Provider>>, cfg: EngineConfig) -> Self {
        Engine { store: Mutex::new(store), providers, fallback: None, tools: tools::all(subagent_list(&cfg), cfg.skills.clone(), cfg.websearch.clone()).into_iter().map(Arc::from).collect(), cfg, auto_failed: Mutex::new(HashSet::new()), prompt_lock: Mutex::new(()), snapshot_off: Mutex::new(HashSet::new()), title_tried: Mutex::new(HashSet::new()), classified: Mutex::new(HashSet::new()), unpriced_noted: Mutex::new(HashSet::new()), me: Mutex::new(std::sync::Weak::new()), jobs: jobs::Jobs::default(), notifier: Mutex::new(None) }
    }

    pub fn catalog(&self) -> &catalog::Catalog {
        &self.cfg.catalog
    }

    pub fn connected_providers(&self) -> Vec<String> {
        let mut v: Vec<String> = self.providers.keys().cloned().collect();
        v.sort();
        v
    }

    /// Provider-wide then model-wide request customization from config.
    fn base_overlay(&self, provider_id: &str, model_id: &str) -> crate::provider::Overlay {
        let mut o = crate::provider::Overlay::default();
        let mut add = |r: &config::RequestOverlay| {
            o.headers.extend(r.headers.iter().map(|(k, v)| (k.clone(), v.clone())));
            if let Some(b) = &r.body {
                match &mut o.body {
                    Some(cur) => crate::provider::effort::merge_json(cur, b),
                    None => o.body = Some(b.clone()),
                }
            }
        };
        if let Some(r) = self.cfg.catalog.providers.get(provider_id).and_then(|p| p.request.as_ref()) {
            add(r);
        }
        if let Some(r) = self.cfg.catalog.model(provider_id, model_id).and_then(|m| m.request.as_ref()) {
            add(r);
        }
        o
    }

    /// Reasoning levels a model offers: the built-in ones for its protocol, then the ones defined in config.
    pub fn effort_levels(&self, provider_id: &str, model_id: &str) -> Vec<String> {
        let cat = &self.cfg.catalog;
        let model = cat.model(provider_id, model_id);
        let mut v = match cat.providers.get(provider_id).and_then(|p| p.protocol()) {
            Some(proto) => crate::provider::effort::variants(proto, provider_id, model_id, model.is_some_and(|m| m.reasoning)),
            None => vec![],
        };
        for name in model.map(|m| m.variants.keys()).into_iter().flatten() {
            if !v.contains(name) {
                v.push(name.clone());
            }
        }
        v
    }

    /// What a front end shows about reasoning effort: the level asked for, whether the model offers it, and the levels.
    pub fn effort_info(&self, session: &Session) -> EffortInfo {
        let agent = agent::find(self.agents_of(session), &session.agent);
        let model_ref = self.model_ref_for(session);
        let (provider_id, model_id) = config::split_model(&model_ref);
        let levels = self.effort_levels(provider_id, model_id);
        let desired = self.desired_effort(session, agent.as_ref());
        let offered = desired.as_ref().is_some_and(|d| levels.contains(d));
        EffortInfo { desired, offered, levels }
    }

    /// The effort a request will use: the session's choice, else the agent's, else the configured default.
    pub fn desired_effort(&self, session: &Session, agent: Option<&Agent>) -> Option<String> {
        session.variant.clone().or_else(|| agent.and_then(|a| a.effort.clone())).or_else(|| self.cfg.effort.clone())
    }

    /// (effort for the adapter, overlay): the desired effort only counts when this model offers it; a variant
    /// defined in config contributes its overlay instead of a built-in effort.
    fn request_extras(&self, session: &Session, agent: &Agent, provider_id: &str, model_id: &str) -> (Option<String>, crate::provider::Overlay) {
        let mut overlay = self.base_overlay(provider_id, model_id);
        let Some(want) = self.desired_effort(session, Some(agent)) else { return (None, overlay) };
        if let Some(v) = self.cfg.catalog.model(provider_id, model_id).and_then(|m| m.variants.get(&want)) {
            overlay.headers.extend(v.headers.iter().map(|(k, v)| (k.clone(), v.clone())));
            if let Some(b) = &v.body {
                match &mut overlay.body {
                    Some(cur) => crate::provider::effort::merge_json(cur, b),
                    None => overlay.body = Some(b.clone()),
                }
            }
            return (None, overlay);
        }
        let offered = self.effort_levels(provider_id, model_id);
        (offered.contains(&want).then_some(want), overlay)
    }

    /// Model used for a session: its own choice, else the agent's, else the configured default.
    pub fn model_ref_for(&self, session: &Session) -> String {
        let agent = agent::find(self.agents_of(session), &session.agent);
        session.model.clone().or(agent.and_then(|a| a.model)).unwrap_or_else(|| self.cfg.model.clone())
    }

    /// The agents of the default permission mode (the agent picker lists these; the names are the same in every mode).
    pub fn agents(&self) -> &[Agent] {
        &self.cfg.agents
    }

    /// The permission mode a session runs in: its own choice, else the configured default.
    pub fn permission_mode_of(&self, session: &Session) -> PermissionMode {
        session.permission_mode.as_deref().and_then(PermissionMode::parse).unwrap_or(self.cfg.permission_mode)
    }

    /// The agents with the rules of the session's permission mode.
    pub fn agents_of(&self, session: &Session) -> &[Agent] {
        let mode = self.permission_mode_of(session);
        self.cfg.agents_by_mode.get(&mode).map(Vec::as_slice).unwrap_or(&self.cfg.agents)
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn spill_dir(&self) -> PathBuf {
        self.cfg.app_dir.join("data/tool-output")
    }

    /// Durably admit a user prompt (steered by default: delivered at the next step boundary).
    pub fn submit(&self, session_id: &str, text: &str, mode: Mode) -> Result<String, StoreError> {
        self.submit_with(session_id, text, None, mode)
    }

    /// Like `submit`, with the expanded `@` mentions (`mentions::expand`) stored alongside the typed text.
    pub fn submit_with(&self, session_id: &str, text: &str, mentions: Option<Value>, mode: Mode) -> Result<String, StoreError> {
        let mut data = json!({ "text": text });
        if let Some(m) = mentions.filter(|m| !m.is_null()) {
            data["mentions"] = m;
        }
        self.store().enqueue(session_id, None, mode, "user", &data)
    }

    /// What the model accepts as input (from the catalog; unknown models are assumed to accept everything).
    pub fn media_caps(&self, provider_id: &str, model_id: &str) -> MediaCaps {
        match self.cfg.catalog.model(provider_id, model_id) {
            Some(m) => MediaCaps { image: m.accepts("image"), pdf: m.accepts("pdf") },
            None => MediaCaps::default(),
        }
    }

    /// The session's current model split into `(provider, model)` and its media capabilities, for the UI.
    pub fn current_media_caps(&self, session: &Session) -> MediaCaps {
        let model_ref = self.model_ref_for(session);
        let (p, m) = config::split_model(&model_ref);
        self.media_caps(p, m)
    }

    pub fn app_dir(&self) -> &std::path::Path {
        &self.cfg.app_dir
    }

    /// Built-in tools plus what the MCP servers offer right now ("each request captures the tool list at request time").
    fn current_tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut all = self.tools.clone();
        if let Some(m) = &self.cfg.mcp {
            all.extend(m.tools());
        }
        all
    }

    pub fn mcp(&self) -> Option<&Arc<crate::mcp::Manager>> {
        self.cfg.mcp.as_ref()
    }

    /// Stored messages as provider messages, with attachments loaded from the blob store.
    fn chat(&self, messages: &[crate::store::Message]) -> Vec<ChatMessage> {
        chat::to_chat_with(messages, &|hash| blobs::get(&self.cfg.app_dir, hash))
    }

    pub fn skills(&self) -> &[skills::Skill] {
        &self.cfg.skills
    }

    /// Names of the agents the subagent tool can start.
    pub fn subagent_names(&self) -> Vec<String> {
        self.cfg.agents.iter().filter(|a| a.subagent()).map(|a| a.id.clone()).collect()
    }

    /// Run until idle. `resume` continues a run that was interrupted by a crash (the claim is still held).
    pub fn run(&self, session_id: &str, ui: &dyn Ui, cancel: &AtomicBool, resume: bool) -> Result<RunOutcome, StoreError> {
        if !resume && !self.store().claim(session_id)? {
            return Ok(RunOutcome::Busy);
        }
        // Sending a new prompt after /undo makes the undo permanent.
        if let Err(e) = self.commit_pending(session_id) {
            self.store().release(session_id)?;
            return Err(e);
        }
        self.auto_failed.lock().unwrap().remove(session_id);
        // An untitled session gets a title from a small model call that runs alongside the first turn.
        let job = self.title_job(session_id);
        let outcome = std::thread::scope(|s| {
            if let Some((session, text)) = &job {
                s.spawn(move || self.generate_title(session, text, ui, cancel));
            }
            self.drain(session_id, ui, cancel, resume)
        });
        self.store().release(session_id)?;
        outcome
    }

    fn drain(&self, sid: &str, ui: &dyn Ui, cancel: &AtomicBool, resume: bool) -> Result<RunOutcome, StoreError> {
        let (mut continuing, mut step) = (resume, 0u32);
        loop {
            let delivered = self.store().deliver(sid, !continuing)?;
            if !delivered.is_empty() {
                step = 0; // new user input resets the step allowance
                ui.event(&UiEvent::Delivered);
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
        let agents = self.agents_of(&session);
        let agent = agent::find(agents, &session.agent).or_else(|| agents.first().cloned()).expect("at least one agent is configured");
        let all_tools = self.current_tools();
        let advertised: Vec<&dyn Tool> = all_tools.iter().map(|t| t.as_ref()).filter(|t| agent::advertised(&agent, *t)).collect();

        let eff = compaction::effective(&history);
        let mut messages = self.chat(&eff);
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
        let skill_list = skills::render_list(&self.cfg.skills);
        let mut system = agent::system_prompt(
            &agent,
            &advertised,
            &agent::EnvInfo { cwd: &session.directory, platform: std::env::consts::OS, date: &agent::date_from_unix(now), instructions: &ins, skills: &skill_list },
        );
        if agent.id == "plan" {
            // In the system prompt (not appended to the last message) so every request keeps the same prefix.
            system.push_str("\n");
            system.push_str(agent::PLAN_REMINDER);
            system.push('\n');
        }
        let (effort, overlay) = self.request_extras(&session, &agent, &provider_id, &model_id);
        let req = Request {
            model: model_id.clone(),
            system,
            messages,
            tools: advertised.iter().map(|t| ToolDef { name: t.name().into(), description: t.description(), schema: t.schema() }).collect(),
            max_tokens: self.cfg.max_tokens,
            tools_disabled: max_steps_hit,
            effort,
            overlay,
        };
        let limit = self.cfg.catalog.model(&provider_id, &model_id).map(|m| m.limit.context).filter(|l| *l > 0);
        Ok(Ok(Prepared { session, agent, req, provider, provider_id, model_id, eff, limit, tools: all_tools }))
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
        let Prepared { session, agent, req, provider, provider_id, model_id, tools: step_tools, .. } = p;
        let (provider_id, model_id) = (provider_id.as_str(), model_id.as_str());
        let caps = self.media_caps(provider_id, model_id);

        let snapshot = self.take_snapshot(&session, ui);
        let mut first = json!({ "parts": [] });
        if let Some(id) = &snapshot {
            first["snapshot"] = json!(id);
        }
        let msg_id = self.store().add_message(sid, "assistant", &first)?;
        let declined = AtomicBool::new(false);
        let permitter = EnginePermitter {
            rules: agent.permissions.clone(),
            project_id: session.project_id.clone(),
            store: &self.store,
            ui,
            declined: &declined,
            ask_lock: &self.prompt_lock,
            classify: self.cfg.classifier.as_ref().filter(|s| s.background || !ui.unattended()).map(|settings| classify::ClassifyCtx { engine: self, session: &session, settings, cancel }),
        };

        let mut retries = 0u32;
        let result = loop {
            let r = self.attempt(provider, &session, &req, &permitter, ui, cancel, &msg_id, caps, &step_tools);
            match &r.stream {
                Err(e) if !r.dispatched && !cancel.load(Ordering::Relaxed) => {
                    // Safe to retry: no tool has run, and the partial output is discarded.
                    retries += 1;
                    if let Some(wait) = retry::delay(retries, e, (self.cfg.jitter)()) {
                        ui.event(&UiEvent::Retry { attempt: retries, wait, error: &e.message() });
                        self.store().set_message_parts(&msg_id, &[])?;
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
        if let Some(id) = &snapshot {
            data["snapshot"] = json!(id);
        }
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

    fn title_job(&self, sid: &str) -> Option<(Session, String)> {
        let session = self.store().session(sid).ok()??;
        if session.title.is_some() || session.parent_id.is_some() {
            return None;
        }
        let (first, latest) = self.store().title_source(sid).ok()??;
        if !self.title_tried.lock().unwrap().insert(sid.to_string()) {
            return None;
        }
        Some((session, title::request_text(&first, latest.as_deref())))
    }

    /// Best effort: any failure just leaves the session untitled. Only errors where the request provably never ran
    /// (connection refused, DNS) are retried, up to 3 times, since this is a one-shot generation.
    fn generate_title(&self, session: &Session, text: &str, ui: &dyn Ui, cancel: &AtomicBool) {
        let model_ref = self.small_model_ref(session);
        let (provider_id, model_id) = config::split_model(&model_ref);
        let Some(provider) = self.providers.get(provider_id).map(|p| p.as_ref()).or(self.fallback.as_deref()) else { return };
        let req = Request {
            model: model_id.to_string(),
            system: title::SYSTEM.into(),
            messages: vec![ChatMessage { role: Role::User, content: vec![Block::Text(text.to_string())] }],
            tools: vec![],
            max_tokens: 60,
            overlay: self.base_overlay(provider_id, model_id),
            ..Default::default()
        };
        let mut attempts = 0;
        let (reply, usage) = loop {
            let (mut out, mut usage) = (String::new(), Usage::default());
            match provider.stream(&req, cancel, &mut |ev| match ev {
                Event::TextDelta(t) => out.push_str(&t),
                Event::Finish { usage: u, .. } => usage = u,
                _ => {}
            }) {
                Ok(()) => break (out, usage),
                Err(ProviderError::Transport { kind: crate::provider::TransportKind::Connect, .. }) if attempts < 3 && !cancel.load(Ordering::Relaxed) => {
                    attempts += 1;
                    (self.cfg.sleep)(Duration::from_secs(1), cancel);
                }
                Err(_) => return,
            }
        };
        let Some(title) = title::clean(&reply) else { return };
        let st = self.store();
        if st.set_title_if_empty(&session.id, &title).unwrap_or(false) {
            let cost = self.cfg.catalog.model(provider_id, model_id).and_then(|m| catalog::cost(m, &usage)).unwrap_or(0.0);
            let _ = st.add_usage(&session.id, cost, &usage);
            drop(st);
            ui.event(&UiEvent::Titled(&title));
        }
    }

    /// Copy the conversation into a new session. `turns_back` = 0 copies everything; N copies the conversation as it
    /// was before the Nth most recent prompt (so the fork can take that prompt in a different direction).
    /// `None` if the session has fewer than N prompts.
    pub fn fork(&self, sid: &str, turns_back: usize) -> Result<Option<Session>, StoreError> {
        let st = self.store();
        let before = if turns_back == 0 {
            None
        } else {
            let hidden_from = st.revert(sid)?.and_then(|r| r["messageId"].as_str().map(String::from));
            let users: Vec<String> = st.messages(sid)?.into_iter().filter(|m| m.kind == "user" && hidden_from.as_deref().is_none_or(|h| m.id.as_str() < h)).map(|m| m.id).collect();
            if users.len() < turns_back {
                return Ok(None);
            }
            Some(users[users.len() - turns_back].clone())
        };
        Ok(Some(st.fork_session(sid, before.as_deref())?))
    }

    /// Run `req` in a child session of `parent` and return the subagent's final text. Used by the `subagent` tool and
    /// by custom commands marked `subagent`.
    pub fn run_subagent(&self, parent: &Session, req: SubagentRequest, ui: &dyn Ui, cancel: &AtomicBool) -> Result<SubagentResult, ToolError> {
        let child = self.begin_subagent(parent, &req)?;
        ui.event(&UiEvent::Status(&format!("subagent {}: {}", child.agent, req.description)));
        self.finish_subagent(&child, ui, cancel)
    }

    /// Create (or find) the child session for `req` and queue its prompt, without running it.
    pub(crate) fn begin_subagent(&self, parent: &Session, req: &SubagentRequest) -> Result<Session, ToolError> {
        let eng = self;
        let store_err = |e: StoreError| ToolError::fail(e.to_string());
        let agent = agent::find(eng.agents_of(parent), &req.agent).filter(|a| a.subagent()).ok_or_else(|| ToolError::fail(format!("Unknown agent '{}'", req.agent)))?;
        let child = {
            let st = eng.store();
            if st.depth(&parent.id).map_err(store_err)? + 1 > MAX_SUBAGENT_DEPTH {
                return Err(ToolError::fail("subagent depth limit reached; do this work yourself"));
            }
            let child = match &req.session_id {
                Some(id) => st
                    .session(id)
                    .map_err(store_err)?
                    .filter(|c| c.parent_id.as_deref() == Some(parent.id.as_str()))
                    .ok_or_else(|| ToolError::fail(format!("sessionID {id} is not a subagent session of this conversation")))?,
                None => {
                    let c = st.create_child_session(parent, &agent.id).map_err(store_err)?;
                    st.set_title(&c.id, &req.description).map_err(store_err)?;
                    // A subagent works under the permission mode its parent had when it started.
                    st.set_permission_mode(&c.id, Some(eng.permission_mode_of(parent).name())).map_err(store_err)?;
                    c
                }
            };
            // A model chosen for this conversation carries over unless the call names one.
            match req.model.as_ref().or(parent.model.as_ref()) {
                Some(m) => st.set_model(&child.id, Some(m)).map_err(store_err)?,
                None => {}
            }
            child
        };
        eng.submit(&child.id, &req.prompt, Mode::Steer).map_err(store_err)?;
        Ok(child)
    }

    /// Run a child session prepared by `begin_subagent` and return its final text.
    pub(crate) fn finish_subagent(&self, child: &Session, ui: &dyn Ui, cancel: &AtomicBool) -> Result<SubagentResult, ToolError> {
        let eng = self;
        let store_err = |e: StoreError| ToolError::fail(e.to_string());
        let child_ui = ChildUi { parent: ui, agent: &child.agent };
        match eng.run(&child.id, &child_ui, cancel, false).map_err(store_err)? {
            RunOutcome::Idle => {}
            RunOutcome::Interrupted => return Err(ToolError::Interrupted),
            RunOutcome::Declined => return Err(ToolError::Declined),
            RunOutcome::Failed(m) => return Err(ToolError::fail(format!("Subagent failed: {m}"))),
            RunOutcome::Busy => return Err(ToolError::fail("that subagent session is already running")),
        }
        Ok(SubagentResult { session_id: child.id.clone(), output: eng.last_assistant_text(&child.id).unwrap_or_else(|| "(the subagent returned no text)".to_string()) })
    }

    /// Text of the latest assistant message that has any.
    pub(crate) fn last_assistant_text(&self, sid: &str) -> Option<String> {
        self.store().messages(sid).ok()?.iter().rev().filter(|m| m.kind == "assistant").find_map(|m| {
            let text: Vec<&str> = m.data["parts"].as_array()?.iter().filter(|p| p["type"] == "text").filter_map(|p| p["text"].as_str()).collect();
            (!text.is_empty()).then(|| text.join("\n\n"))
        })
    }

    /// Capture the working tree before a step. Best effort: a failure disables snapshots for the session with one notice.
    fn take_snapshot(&self, session: &Session, ui: &dyn Ui) -> Option<String> {
        if !self.cfg.snapshots || self.snapshot_off.lock().unwrap().contains(&session.id) {
            return None;
        }
        let prev = {
            let st = self.store();
            st.last_snapshot(&session.id).ok().flatten().or_else(|| session.parent_id.as_ref().and_then(|p| st.last_snapshot(p).ok().flatten()))
        };
        match self.snaps(session).capture(std::path::Path::new(&session.directory), prev.as_deref()) {
            Ok(id) => Some(id),
            Err(e) => {
                self.snapshot_off.lock().unwrap().insert(session.id.clone());
                ui.event(&UiEvent::Notice(&format!("snapshots are off for this session (undo and diff unavailable): {e}")));
                None
            }
        }
    }

    fn snaps(&self, session: &Session) -> Snapshots {
        let mut s = Snapshots::new(self.cfg.app_dir.join("data/snapshots").join(&session.project_id));
        s.max_files = self.cfg.snapshot_max_files;
        s
    }

    /// Remove old snapshot data that no recent session refers to. Returns (manifests, blobs) removed.
    pub fn prune_snapshots(&self, project_ids: &[String], max_age: Duration) -> (usize, usize) {
        let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0) - max_age.as_millis() as i64;
        let mut total = (0, 0);
        for pid in project_ids {
            let keep = self.store().live_snapshots(pid, since).unwrap_or_default();
            let (m, b) = Snapshots::new(self.cfg.app_dir.join("data/snapshots").join(pid)).prune(&keep, max_age);
            total = (total.0 + m, total.1 + b);
        }
        total
    }

    /// Undo the latest turn (user message and everything after): files go back to how they were before the turn's
    /// first step, the messages are hidden. Repeat to go further back. `redo` reverses it; the next prompt commits it.
    pub fn undo(&self, sid: &str) -> Result<UndoResult, StoreError> {
        if !self.store().claim(sid)? {
            return Ok(UndoResult::Busy);
        }
        let r = self.undo_inner(sid);
        self.store().release(sid)?;
        r
    }

    fn undo_inner(&self, sid: &str) -> Result<UndoResult, StoreError> {
        let (session, msgs, revert) = {
            let st = self.store();
            (st.session(sid)?.expect("session exists"), st.messages(sid)?, st.revert(sid)?)
        };
        let marker = revert.as_ref().and_then(|r| r["messageId"].as_str()).map(String::from);
        let Some(turn) = msgs.iter().filter(|m| m.kind == "user" && marker.as_deref().is_none_or(|mk| m.id.as_str() < mk)).last() else {
            return Ok(UndoResult::NothingToUndo);
        };
        let snapshot = msgs.iter().filter(|m| m.kind == "assistant" && m.id > turn.id).find_map(|m| m.data["snapshot"].as_str().map(String::from));
        let mut redo = revert.as_ref().and_then(|r| r["redo"].as_str()).map(String::from);
        let mut report = UndoReport { hidden_messages: msgs.iter().filter(|m| m.id >= turn.id && (m.kind == "user" || m.kind == "assistant")).count(), ..Default::default() };
        if let (true, Some(snap)) = (self.cfg.snapshots, snapshot) {
            let hint = self.store().last_snapshot(sid)?;
            match self.snaps(&session).restore(std::path::Path::new(&session.directory), &snap, hint.as_deref()) {
                Ok(r) => {
                    report.restored = r.restored;
                    report.deleted = r.deleted;
                    report.files_restored = true;
                    redo = redo.or(Some(r.before)); // the state before the first undo is what redo returns to
                }
                Err(e) => return Ok(UndoResult::Failed(e.to_string())),
            }
        }
        self.store().set_revert(sid, Some(&json!({ "messageId": turn.id, "redo": redo })))?;
        Ok(UndoResult::Done(report))
    }

    /// Reverse staged undo(s): files and messages come back as they were before the first `undo`.
    pub fn redo(&self, sid: &str) -> Result<UndoResult, StoreError> {
        if !self.store().claim(sid)? {
            return Ok(UndoResult::Busy);
        }
        let r = (|| {
            let (session, revert) = {
                let st = self.store();
                (st.session(sid)?.expect("session exists"), st.revert(sid)?)
            };
            let Some(revert) = revert else { return Ok(UndoResult::NothingToUndo) };
            let mut report = UndoReport::default();
            if let (true, Some(redo)) = (self.cfg.snapshots, revert["redo"].as_str()) {
                match self.snaps(&session).restore(std::path::Path::new(&session.directory), redo, None) {
                    Ok(r) => {
                        report.restored = r.restored;
                        report.deleted = r.deleted;
                        report.files_restored = true;
                    }
                    Err(e) => return Ok(UndoResult::Failed(e.to_string())),
                }
            }
            self.store().set_revert(sid, None)?;
            Ok(UndoResult::Done(report))
        })();
        self.store().release(sid)?;
        r
    }

    /// Make a staged undo permanent: the hidden messages are deleted.
    pub fn commit_pending(&self, sid: &str) -> Result<usize, StoreError> {
        let st = self.store();
        let Some(revert) = st.revert(sid)? else { return Ok(0) };
        let n = match revert["messageId"].as_str() {
            Some(id) => st.delete_messages_from(sid, id)?,
            None => 0,
        };
        st.set_revert(sid, None)?;
        Ok(n)
    }

    /// Changes in the working tree since the session's first step.
    pub fn session_diff(&self, sid: &str) -> Result<DiffResult, StoreError> {
        let (session, msgs, last) = {
            let st = self.store();
            (st.session(sid)?.expect("session exists"), st.messages(sid)?, st.last_snapshot(sid)?)
        };
        let Some(first) = msgs.iter().filter(|m| m.kind == "assistant").find_map(|m| m.data["snapshot"].as_str().map(String::from)) else {
            return Ok(DiffResult::NoSnapshots);
        };
        let snaps = self.snaps(&session);
        let now = match snaps.capture(std::path::Path::new(&session.directory), last.as_deref()) {
            Ok(id) => id,
            Err(e) => return Ok(DiffResult::Failed(e.to_string())),
        };
        Ok(snaps.diff(&first, &now).map_or_else(|e| DiffResult::Failed(e.to_string()), DiffResult::Files))
    }

    /// Manual compaction (`/compact`). Holds the session claim while it runs.
    pub fn compact(&self, sid: &str, ui: &dyn Ui, cancel: &AtomicBool) -> Result<CompactResult, StoreError> {
        if !self.store().claim(sid)? {
            return Ok(CompactResult::Failed("session is busy".into()));
        }
        let r = self.commit_pending(sid).and_then(|_| self.compact_inner(sid, "manual", ui, cancel));
        self.store().release(sid)?;
        r
    }

    /// Summarize everything before a cut point into a checkpoint message (see `compaction.rs`).
    fn compact_inner(&self, sid: &str, trigger: &str, ui: &dyn Ui, cancel: &AtomicBool) -> Result<CompactResult, StoreError> {
        let (session, history) = {
            let st = self.store();
            (st.session(sid)?.expect("session exists"), st.messages(sid)?)
        };
        let agent = agent::find(self.agents_of(&session), &session.agent).or_else(|| self.agents_of(&session).first().cloned()).expect("an agent");
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
            let mut messages = self.chat(&portion);
            append_user_text(&mut messages, compaction::prompt(update));
            let req = Request { model: model_id.clone(), system: compaction::SYSTEM.into(), messages, max_tokens: self.cfg.max_tokens, overlay: self.base_overlay(&provider_id, &model_id), ..Default::default() };
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
    #[allow(clippy::too_many_arguments)]
    fn attempt(&self, provider: &dyn Provider, session: &Session, req: &Request, permitter: &EnginePermitter, ui: &dyn Ui, cancel: &AtomicBool, msg_id: &str, caps: MediaCaps, step_tools: &[Arc<dyn Tool>]) -> AttemptResult {
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
                Event::Native { provider, data } => {
                    text_idx = None;
                    parts.push(json!({"type":"native","provider":provider,"data":data}));
                }
                Event::ToolCall { id, name, input } => {
                    text_idx = None;
                    dispatched = true;
                    ui.event(&UiEvent::ToolStarted { name: &name, input: &input });
                    parts.push(chat::tool_part(&id, &name, &input));
                    let idx = parts.len() - 1;
                    let _ = self.store().set_message_parts(msg_id, &parts);
                    let tool = step_tools.iter().find(|t| t.name() == name).map(|t| t.as_ref());
                    let (cwd, root, spill) = (cwd.clone(), root.clone(), spill.clone());
                    handles.push((
                        idx,
                        s.spawn(move || match tool {
                            None => Err(ToolError::fail(format!("Unknown tool: {name}"))),
                            Some(t) => {
                                let host = EngineHost { engine: self, session, ui, cancel };
                                t.run(&input, &ToolCtx { formatters: &self.cfg.formatters, media: caps, session_id: &session.id, host: &host, cwd, project_root: root, spill_dir: spill, permitter, cancel })
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
            Ok(mut out) => {
                // Images and PDFs a tool returned go to the blob store; the message keeps a small reference.
                let mut media = vec![];
                for a in out.take_media() {
                    match blobs::put(&self.cfg.app_dir, &a.bytes) {
                        Ok(hash) => media.push(json!({"mediaType": a.media_type, "blob": hash, "size": a.bytes.len(), "name": a.name})),
                        Err(e) => out.text.push_str(&format!("\n[could not store the attachment {}: {e}]", a.name)),
                    }
                }
                let text = crate::truncate::truncate(&out.text, spill).map(|t| t.text).unwrap_or(out.text);
                let text = if text.is_empty() { "(no output)".to_string() } else { text };
                ui.event(&UiEvent::ToolFinished { name, ok: true, text: &text });
                let mut state = json!({"status":"completed","output":text,"metadata":out.metadata});
                if !media.is_empty() {
                    state["media"] = json!(media);
                }
                state
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
        self.engine.run_subagent(self.session, req, self.ui, self.cancel)
    }

    fn start_background_shell(&self, command: &str, workdir: &std::path::Path, timeout: Option<Duration>, unsandboxed: bool) -> Result<(String, PathBuf), ToolError> {
        let eng = self.engine.shared()?;
        eng.start_shell_job(self.session, command, workdir, timeout, unsandboxed)
    }

    fn start_background_subagent(&self, req: SubagentRequest) -> Result<String, ToolError> {
        let eng = self.engine.shared()?;
        let child = eng.begin_subagent(self.session, &req)?;
        eng.start_subagent_job(self.session, &child, &req.description);
        Ok(child.id)
    }

    fn jobs(&self) -> Vec<tools::JobInfo> {
        self.engine.background_jobs(Some(&self.session.id))
    }

    fn job_output(&self, id: &str, lines: usize) -> Result<String, ToolError> {
        self.engine.job_output(id, lines).map_err(ToolError::fail)
    }

    fn kill_job(&self, id: &str) -> Result<String, ToolError> {
        self.engine.kill_job(id).map_err(ToolError::fail)
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
    /// Present when the command classifier is on for this step.
    classify: Option<classify::ClassifyCtx<'a>>,
}

impl EnginePermitter<'_> {
    /// The classifier may answer for a shell command that would only ask because of a default rule. It never
    /// overrides a rule the user or the careful preset's always-ask list wrote, and never sees opaque or dangerous
    /// commands. `true` = run it without asking.
    fn auto_allowed(&self, req: &PermReq) -> bool {
        let (Some(c), Some(command)) = (&self.classify, &req.command) else { return false };
        if req.action != "shell" || req.force_ask {
            return false;
        }
        let parsed = tools::shellparse::parse(command);
        if classifier::never_classify(&parsed) {
            return false;
        }
        let saved = self.store.lock().unwrap_or_else(|e| e.into_inner()).saved_permissions(&self.project_id).unwrap_or_default();
        if !permission::only_default_asks(&req.action, &req.resources, &self.rules, &saved) {
            return false;
        }
        match c.engine.classify_command(c.session, command, &parsed, c.settings, self.ui, c.cancel) {
            classifier::Verdict::Allow { risk, reason } => {
                let shown = classifier::sanitize(command, 100);
                self.ui.event(&UiEvent::Notice(&if reason.is_empty() { format!("auto: {risk}: {shown}") } else { format!("auto: {risk}: {shown} ({reason})") }));
                true
            }
            classifier::Verdict::Ask { reason } => {
                if !reason.is_empty() {
                    self.ui.event(&UiEvent::Notice(&format!("classifier: asking you because {reason}")));
                }
                false
            }
        }
    }

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
                if self.auto_allowed(req) {
                    return Ok(());
                }
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
        /// `Delivered` events are counted, not logged, so event-sequence assertions stay about the model's output.
        delivered: std::sync::atomic::AtomicUsize,
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
            UiEvent::ClassifierUsage { .. } => "classifier-usage".into(),
            UiEvent::Notice(n) => format!("notice:{n}"),
            UiEvent::Status(n) => format!("status:{n}"),
            UiEvent::Todos(t) => format!("todos:{}", t.len()),
            UiEvent::Titled(t) => format!("title:{t}"),
            UiEvent::Delivered => "delivered".into(),
            UiEvent::Nested { agent, inner } => format!("{agent}>{}", describe(inner)),
        }
    }

    impl Ui for TestUi {
        fn event(&self, e: &UiEvent) {
            if matches!(e, UiEvent::Delivered) {
                self.delivered.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
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
        // Engine mechanics are tested without permission prompts; prompting tests set their own rules in `tweak`.
        cfg.agents = crate::agent::builtin_for(crate::permission::PermissionMode::Auto);
        tweak(&mut cfg);
        let store = Store::open_memory().unwrap();
        let session = store.create_session(dir.join("work").to_str().unwrap(), "build").unwrap();
        let engine = Engine::new(store, Box::new(Shared(fake.clone())), cfg).share();
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
        assert_eq!(req.tools.len(), 12);
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
    fn delivery_is_announced_to_the_ui_in_order_with_the_output() {
        let f = fixture(vec![text("one"), text("two")]);
        f.say("first");
        f.engine.submit(&f.session.id, "second", Mode::Queue).unwrap();
        let ui = TestUi::default();
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        // Once for "first", once for the queued "second".
        assert_eq!(ui.delivered.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[test]
    fn reasoning_effort_follows_session_then_agent_then_config_and_only_when_the_model_offers_it() {
        let f = fixture_cfg(vec![text("a"), text("b"), text("c"), text("d")], |c| {
            c.model = "anthropic/claude-opus-5-5".into();
            c.effort = Some("low".into());
        });
        let effort = |n: usize| f.fake.requests.lock().unwrap()[n].effort.clone();
        f.say("one");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        assert_eq!(effort(0).as_deref(), Some("low"), "the configured default");
        f.engine.store().set_variant(&f.session.id, Some("max")).unwrap();
        f.say("two");
        f.run(&TestUi::default());
        assert_eq!(effort(1).as_deref(), Some("max"), "the session's choice wins");
        f.engine.store().set_variant(&f.session.id, Some("none")).unwrap();
        f.say("three");
        f.run(&TestUi::default());
        assert_eq!(effort(2), None, "Opus 5.5 cannot think less than 'low': a level it does not offer is ignored, not an error");
        f.engine.store().set_variant(&f.session.id, None).unwrap();
        f.say("four");
        f.run(&TestUi::default());
        assert_eq!(effort(3).as_deref(), Some("low"));
        let s = f.engine.store().session(&f.session.id).unwrap().unwrap();
        assert_eq!(f.engine.effort_levels("anthropic", "claude-opus-5-5"), ["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(f.engine.desired_effort(&s, None).as_deref(), Some("low"));
        let mut a = agent::find(&f.engine.cfg.agents, "build").unwrap();
        a.effort = Some("high".into());
        assert_eq!(f.engine.desired_effort(&s, Some(&a)).as_deref(), Some("high"), "the agent's default beats the global one");
    }

    #[test]
    fn config_overlays_and_custom_variants_reach_the_request() {
        use crate::config::RequestOverlay;
        let f = fixture_cfg(vec![text("a"), text("b")], |c| {
            c.model = "anthropic/claude-opus-5-5".into();
            let p = c.catalog.providers.get_mut("anthropic").unwrap();
            p.request = Some(RequestOverlay { headers: [("x-proxy".to_string(), "p".to_string())].into(), body: Some(json!({"metadata": {"user_id": "u"}})) });
            let m = p.models.entry("claude-opus-5-5".into()).or_default();
            m.request = Some(RequestOverlay { headers: Default::default(), body: Some(json!({"metadata": {"team": "t"}})) });
            m.variants.insert("deep".into(), RequestOverlay { headers: [("x-mode".to_string(), "deep".to_string())].into(), body: Some(json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "xhigh"}})) });
        });
        f.say("one");
        f.run(&TestUi::default());
        let o = f.fake.requests.lock().unwrap()[0].overlay.clone();
        assert_eq!(o.headers, [("x-proxy".to_string(), "p".to_string())]);
        assert_eq!(o.body, Some(json!({"metadata": {"user_id": "u", "team": "t"}})), "provider overlay first, the model's merged over it");
        assert!(f.engine.effort_levels("anthropic", "claude-opus-5-5").contains(&"deep".to_string()), "custom levels are listed after the built-in ones");
        f.engine.store().set_variant(&f.session.id, Some("deep")).unwrap();
        f.say("two");
        f.run(&TestUi::default());
        let r = f.fake.requests.lock().unwrap()[1].clone();
        assert_eq!(r.effort, None, "a custom variant is an overlay, not a built-in effort");
        assert_eq!(r.overlay.headers.len(), 2);
        assert_eq!(r.overlay.body.unwrap()["output_config"], json!({"effort": "xhigh"}));
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
        assert_eq!(reqs[1].tools.len(), 12);
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
        let f = fixture_cfg(script(), |c| c.agents = config::resolve_agents_in(&cfg, crate::permission::PermissionMode::Auto));
        f.say("go");
        let ui = TestUi::replying(vec![Reply::Once]);
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        assert_eq!(ui.asked.lock().unwrap().as_slice(), ["shell echo hi"]);
        // Rejecting inside the child stops the whole run.
        let f = fixture_cfg(script(), |c| c.agents = config::resolve_agents_in(&cfg, crate::permission::PermissionMode::Auto));
        f.say("go");
        let ui = TestUi::replying(vec![Reply::Reject(None)]);
        assert_eq!(f.run(&ui), RunOutcome::Declined);
        assert_eq!(f.fake.calls(), 2, "no further model calls after the rejection");
    }

    #[test]
    fn the_session_permission_mode_decides_what_asks() {
        let f = fixture_cfg(
            vec![tool_call("t1", "shell", json!({"command":"true"})), text("ok"), tool_call("t2", "shell", json!({"command":"true"})), text("done")],
            |c| {
                c.permission_mode = PermissionMode::Manual;
                c.agents_by_mode = BTreeMap::from([(PermissionMode::Manual, agent::builtin_for(PermissionMode::Manual)), (PermissionMode::Auto, agent::builtin_for(PermissionMode::Auto))]);
            },
        );
        // Auto: the shell command runs without asking (the UI would panic on a prompt).
        f.engine.store().set_permission_mode(&f.session.id, Some("auto")).unwrap();
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        // Switched to manual: the next command asks, and the rejection stops the run.
        f.engine.store().set_permission_mode(&f.session.id, Some("manual")).unwrap();
        f.say("again");
        assert_eq!(f.run(&TestUi::replying(vec![Reply::Reject(None)])), RunOutcome::Declined);
    }

    // --- command classifier (classifier.rs) ---

    fn verdict(v: &str, risk: &str) -> Scripted {
        text(&format!(r#"{{"verdict":"{v}","risk":"{risk}","reason":"because"}}"#))
    }

    fn manual_with(settings: Option<classifier::Settings>, more: Vec<Rule>) -> impl FnOnce(&mut EngineConfig) {
        move |c| {
            let mut manual = agent::builtin_for(PermissionMode::Manual);
            for a in &mut manual {
                a.permissions.extend(more.iter().cloned());
            }
            c.permission_mode = PermissionMode::Manual;
            c.agents_by_mode = BTreeMap::from([(PermissionMode::Manual, manual), (PermissionMode::Auto, agent::builtin_for(PermissionMode::Auto))]);
            // A model with a catalog price, so cost is real (and the no-price notice stays out of the way).
            c.small_model = Some(PRICED.into());
            c.classifier = settings;
        }
    }

    const PRICED: &str = "anthropic/claude-haiku-5-5";

    fn shell_twice(cmd: &str) -> Vec<Scripted> {
        vec![tool_call("t1", "shell", json!({"command": cmd})), tool_call("t2", "shell", json!({"command": cmd})), text("done")]
    }

    fn notices(ui: &TestUi) -> Vec<String> {
        ui.log().into_iter().filter(|l| l.starts_with("notice:")).collect()
    }

    #[test]
    fn the_classifier_allows_a_routine_command_once_and_remembers_it() {
        let f = fixture_cfg(shell_twice("echo hi"), manual_with(Some(Default::default()), vec![]));
        f.fake.script_verdicts(vec![verdict("allow", "read-only")]);
        f.say("go");
        let ui = TestUi::default(); // a prompt would panic
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        assert_eq!(f.fake.classifier_requests.lock().unwrap().len(), 1, "the second identical command is not classified again");
        let n = notices(&ui);
        assert_eq!(n[0], "notice:auto: read-only: echo hi (because)");
        assert_eq!(n[1], "notice:auto: allowed earlier: echo hi");
        let req = f.fake.classifier_requests.lock().unwrap()[0].clone();
        assert_eq!(req.system, classifier::SYSTEM);
        assert!(req.tools.is_empty() && req.messages.len() == 1, "no conversation history goes to the classifier");
        let input: Value = serde_json::from_str(&all_text(&req)).unwrap();
        assert_eq!(input["command"], "echo hi");
        assert!(input["cwd"].as_str().unwrap().ends_with("/work"));
        // Three main steps and one classifier call, all on the session's bill.
        assert_eq!(f.engine.store().usage_totals(&f.session.id).unwrap().1.input, 40);
    }

    #[test]
    fn classifier_spend_is_priced_reported_at_once_and_counted_in_the_session() {
        let f = fixture_cfg(shell_twice("echo hi"), |c| {
            manual_with(Some(Default::default()), vec![])(c);
            c.model = PRICED.into(); // the three main steps cost money too
        });
        f.fake.script_verdicts(vec![verdict("allow", "read-only")]);
        f.say("go");
        let seen = Mutex::new(vec![]);
        struct Spy<'a>(&'a TestUi, &'a Mutex<Vec<(f64, f64, f64)>>);
        impl Ui for Spy<'_> {
            fn event(&self, e: &UiEvent) {
                if let UiEvent::ClassifierUsage { cost, session_cost, classifier_cost } = e {
                    self.1.lock().unwrap().push((*cost, *session_cost, *classifier_cost));
                }
                self.0.event(e)
            }
            fn ask(&self, p: &PermissionPrompt) -> Reply {
                self.0.ask(p)
            }
        }
        let inner = TestUi::default();
        assert_eq!(f.engine.run(&f.session.id, &Spy(&inner, &seen), &AtomicBool::new(false), false).unwrap(), RunOutcome::Idle);
        // 10 input and 5 output tokens at $0.10 / $0.50 per million, for the one call that was made.
        let call = (10.0 * 0.1 + 5.0 * 0.5) / 1e6;
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "the cached second command costs nothing and reports nothing");
        assert!((seen[0].0 - call).abs() < 1e-12 && (seen[0].2 - call).abs() < 1e-12, "{seen:?}");
        assert!((seen[0].1 - call).abs() < 1e-12, "when the call is made, no step has finished yet: {seen:?}");
        let st = f.engine.store();
        let (total, _) = st.usage_totals(&f.session.id).unwrap();
        assert!((total - 4.0 * call).abs() < 1e-12, "three steps and one classifier call, all in the total: {total}");
        assert!((st.classifier_cost(&f.session.id).unwrap() - call).abs() < 1e-12, "the classifier's share is shown on its own");
        assert!(!inner.log().iter().any(|l| l.contains("no price")));
    }

    #[test]
    fn a_classifier_model_without_a_price_is_said_so_once() {
        let f = fixture_cfg(shell_twice("echo hi"), |c| {
            manual_with(Some(classifier::Settings { model: Some("anthropic/claude-unpriced".into()), ..Default::default() }), vec![])(c);
        });
        f.fake.script_verdicts(vec![verdict("ask", "unknown"), verdict("ask", "unknown")]);
        f.say("go");
        let ui = TestUi::replying(vec![Reply::Once, Reply::Once]);
        f.run(&ui);
        assert_eq!(f.fake.classifier_requests.lock().unwrap().len(), 2);
        let said: Vec<String> = ui.log().into_iter().filter(|l| l.contains("no price")).collect();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("anthropic/claude-unpriced") && said[0].contains("$0.00"));
        assert_eq!(f.engine.store().classifier_cost(&f.session.id).unwrap(), 0.0);
    }

    #[test]
    fn a_subagents_classifier_spend_rolls_up_to_its_parent() {
        let st = Store::open_memory().unwrap();
        let parent = st.create_session("/tmp/p", "build").unwrap();
        let child = st.create_child_session(&parent, "general").unwrap();
        let u = Usage { input: 1, ..Default::default() };
        st.add_usage(&child.id, 0.5, &u).unwrap();
        st.add_classifier_usage(&child.id, 0.25, &u).unwrap();
        assert_eq!((st.classifier_cost(&child.id).unwrap(), st.classifier_cost(&parent.id).unwrap()), (0.25, 0.25));
        assert_eq!(st.usage_totals(&parent.id).unwrap().0, 0.75);
    }

    #[test]
    fn anything_but_a_safe_allow_asks_the_user() {
        for (reply, why) in [
            (Some(verdict("allow", "network")), "network: because"),
            (Some(verdict("ask", "unknown")), "because"),
            (Some(verdict("deny", "destructive")), "the classifier advises against this: because"),
            (Some(text("I think it is fine")), "the classifier gave no usable answer"),
            (None, "the classifier gave no usable answer"),
        ] {
            let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "make"})), text("done")], manual_with(Some(Default::default()), vec![]));
            f.fake.script_verdicts(reply.into_iter().collect());
            f.say("go");
            let ui = TestUi::replying(vec![Reply::Once]);
            assert_eq!(f.run(&ui), RunOutcome::Idle, "{why}");
            assert_eq!(ui.asked.lock().unwrap().len(), 1, "{why}");
            assert!(notices(&ui).contains(&format!("notice:classifier: asking you because {why}")), "{why}: {:?}", notices(&ui));
        }
        // A refusal is remembered nowhere: the same command is classified (and asked) again.
        let f = fixture_cfg(shell_twice("make"), manual_with(Some(Default::default()), vec![]));
        f.fake.script_verdicts(vec![verdict("ask", "unknown"), verdict("ask", "unknown")]);
        f.say("go");
        let ui = TestUi::replying(vec![Reply::Once, Reply::Once]);
        f.run(&ui);
        assert_eq!(f.fake.classifier_requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn dangerous_opaque_and_explicitly_ruled_commands_never_reach_the_classifier() {
        for (cmd, rules) in [
            ("git push origin main", vec![]),
            ("rm -rf build", vec![]),
            ("sudo make install", vec![]),
            ("echo $(whoami)", vec![]),
            ("make test", vec![Rule::new("shell", "make *", Effect::Ask)]),
            ("make test", vec![Rule::new("shell", "*", Effect::Ask)]),
        ] {
            let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": cmd})), text("done")], manual_with(Some(Default::default()), rules));
            f.fake.script_verdicts(vec![verdict("allow", "read-only")]);
            f.say("go");
            let ui = TestUi::replying(vec![Reply::Reject(None)]);
            assert_eq!(f.run(&ui), RunOutcome::Declined, "{cmd}");
            assert_eq!(ui.asked.lock().unwrap().len(), 1, "{cmd}");
            assert!(f.fake.classifier_requests.lock().unwrap().is_empty(), "{cmd}");
        }
    }

    #[test]
    fn a_rule_based_deny_stays_a_deny_and_the_classifier_is_off_by_default() {
        let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "make"})), text("done")], manual_with(Some(Default::default()), vec![Rule::new("shell", "make", Effect::Deny)]));
        f.fake.script_verdicts(vec![verdict("allow", "read-only")]);
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        assert!(matches!(&last_user_blocks(&f.fake.requests.lock().unwrap()[1])[0], Block::ToolResult { is_error: true, content, .. } if content.starts_with("Permission denied: shell")));
        assert!(f.fake.classifier_requests.lock().unwrap().is_empty());

        let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "echo hi"})), text("done")], manual_with(None, vec![]));
        f.say("go");
        let ui = TestUi::replying(vec![Reply::Once]);
        f.run(&ui);
        assert_eq!(ui.asked.lock().unwrap().len(), 1);
        assert!(f.fake.classifier_requests.lock().unwrap().is_empty());
    }

    #[test]
    fn a_slow_classifier_times_out_into_a_prompt() {
        let settings = classifier::Settings { timeout: Duration::from_millis(150), ..Default::default() };
        let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "echo hi"})), text("done")], manual_with(Some(settings), vec![]));
        f.fake.delay_classifier(Duration::from_secs(30));
        f.fake.script_verdicts(vec![verdict("allow", "read-only")]);
        f.say("go");
        let ui = TestUi::replying(vec![Reply::Once]);
        let start = std::time::Instant::now();
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        assert!(start.elapsed() < Duration::from_secs(10));
        assert_eq!(ui.asked.lock().unwrap().len(), 1);
        assert!(notices(&ui).iter().any(|n| n.contains("did not answer in time")));
    }

    #[test]
    fn the_classifier_model_comes_from_its_setting_then_the_small_model() {
        let settings = classifier::Settings { model: Some("anthropic/claude-judge".into()), ..Default::default() };
        let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "echo hi"})), text("done")], |c| {
            manual_with(Some(settings), vec![])(c);
            c.small_model = Some("anthropic/claude-small".into());
        });
        f.fake.script_verdicts(vec![verdict("allow", "read-only")]);
        f.say("go");
        f.run(&TestUi::default());
        assert_eq!(f.fake.classifier_requests.lock().unwrap()[0].model, "claude-judge");

        let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "echo hi"})), text("done")], |c| {
            manual_with(Some(Default::default()), vec![])(c);
            c.small_model = Some("anthropic/claude-small".into());
        });
        f.fake.script_verdicts(vec![verdict("allow", "read-only")]);
        f.say("go");
        f.run(&TestUi::default());
        assert_eq!(f.fake.classifier_requests.lock().unwrap()[0].model, "claude-small");
    }

    /// A UI nobody is behind, like a background subagent's.
    struct Away;
    impl Ui for Away {
        fn event(&self, _: &UiEvent) {}
        fn ask(&self, _: &PermissionPrompt) -> Reply {
            Reply::Reject(Some("nobody here".into()))
        }
        fn unattended(&self) -> bool {
            true
        }
    }

    #[test]
    fn unattended_runs_use_the_classifier_only_when_background_is_on() {
        for (background, runs_unprompted) in [(true, true), (false, false)] {
            let settings = classifier::Settings { background, ..Default::default() };
            let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "echo hi"})), text("done")], manual_with(Some(settings), vec![]));
            f.fake.script_verdicts(vec![verdict("allow", "read-only")]);
            f.say("go");
            assert_eq!(f.engine.run(&f.session.id, &Away, &AtomicBool::new(false), false).unwrap(), RunOutcome::Idle);
            let reqs = f.fake.requests.lock().unwrap();
            let result = &last_user_blocks(&reqs[1])[0];
            assert_eq!(matches!(result, Block::ToolResult { is_error: false, .. }), runs_unprompted, "background={background}");
            assert_eq!(f.fake.classifier_requests.lock().unwrap().len(), usize::from(background));
        }
    }

    #[test]
    fn a_subagent_inherits_its_parents_permission_mode() {
        let f = fixture(vec![sub("t1", "general", "do it"), text("child"), text("parent done")]);
        f.engine.store().set_permission_mode(&f.session.id, Some("auto")).unwrap();
        f.say("delegate");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        let child = f.engine.store().session(&child_id(&reqs[2])).unwrap().unwrap();
        assert_eq!(child.permission_mode.as_deref(), Some("auto"));
    }

    #[test]
    fn explore_is_read_only() {
        let f = fixture(vec![sub("t1", "explore", "look"), tool_call("c1", "write", json!({"path":"x.txt","content":"c"})), text("could not write"), text("done")]);
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        let names: Vec<&str> = reqs[1].tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["read", "glob", "grep", "webfetch"]);
        assert!(reqs[1].system.contains("file search specialist"));
        assert!(matches!(&reqs[2].messages.last().unwrap().content[0], Block::ToolResult { is_error: true, content, .. } if content.starts_with("Permission denied: edit")));
        assert!(!f.dir.join("work/x.txt").exists());
    }

    #[test]
    fn subagent_depth_is_limited() {
        let cfg: config::Config = serde_json::from_value(json!({"agents":{"deep":{"mode":"all","description":"recurses"}}})).unwrap();
        let mut script: Vec<Scripted> = (0..4).map(|i| sub(&format!("t{i}"), "deep", "go deeper")).collect();
        script.extend([text("c3"), text("c2"), text("c1"), text("p")]);
        let f = fixture_cfg(script, |c| c.agents = config::resolve_agents_in(&cfg, crate::permission::PermissionMode::Auto));
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

    fn write_call(id: &str, path: &str, content: &str) -> Event {
        Event::ToolCall { id: id.into(), name: "write".into(), input: json!({"path": path, "content": content}) }
    }
    fn tool_step(calls: Vec<Event>) -> Scripted {
        let mut ev = calls;
        ev.push(Event::Finish { reason: FinishReason::ToolUse, usage: Usage::default() });
        Scripted::Events(ev)
    }
    fn read_work(f: &Fixture, rel: &str) -> Option<String> {
        std::fs::read_to_string(f.dir.join("work").join(rel)).ok()
    }
    fn two_turns() -> Fixture {
        let f = fixture(vec![
            tool_step(vec![write_call("w1", "a.txt", "v1")]),
            text("done one"),
            tool_step(vec![write_call("w2", "a.txt", "v2"), write_call("w3", "b.txt", "B")]),
            text("done two"),
        ]);
        f.say("one");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        f.say("two");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        assert_eq!((read_work(&f, "a.txt").as_deref(), read_work(&f, "b.txt").as_deref()), (Some("v2"), Some("B")));
        f
    }

    #[test]
    fn every_step_records_the_state_before_it_ran() {
        let f = two_turns();
        let snaps: Vec<Option<String>> = f.msgs().iter().filter(|m| m.kind == "assistant").map(|m| m.data["snapshot"].as_str().map(String::from)).collect();
        assert_eq!(snaps.len(), 4);
        assert!(snaps.iter().all(Option::is_some));
        // Step 1 saw an empty directory, step 3 saw what turn one left behind (same as step 2's start).
        let s = f.engine.snaps(&f.engine.store().session(&f.session.id).unwrap().unwrap());
        let files = |i: usize| s.manifest(snaps[i].as_ref().unwrap()).unwrap().files.keys().cloned().collect::<Vec<_>>();
        assert_eq!((files(0), files(1), files(2), files(3)), (vec![], vec!["a.txt".to_string()], vec!["a.txt".to_string()], vec!["a.txt".to_string(), "b.txt".to_string()]));
        assert_eq!(snaps[1], snaps[2], "identical trees share one snapshot id");
    }

    #[test]
    fn undo_redo_and_commit_with_real_files() {
        let f = two_turns();
        let sid = &f.session.id;
        // Undo turn two: a.txt back to v1, b.txt gone.
        let UndoResult::Done(r) = f.engine.undo(sid).unwrap() else { panic!() };
        assert_eq!((r.restored, r.deleted, r.hidden_messages, r.files_restored), (vec!["a.txt".to_string()], vec!["b.txt".to_string()], 3, true));
        assert_eq!((read_work(&f, "a.txt").as_deref(), read_work(&f, "b.txt")), (Some("v1"), None));
        // Undo turn one as well: a.txt did not exist before.
        let UndoResult::Done(r) = f.engine.undo(sid).unwrap() else { panic!() };
        assert_eq!((r.deleted, r.hidden_messages), (vec!["a.txt".to_string()], 6));
        assert_eq!(read_work(&f, "a.txt"), None);
        assert_eq!(f.engine.undo(sid).unwrap(), UndoResult::NothingToUndo);
        // Redo returns to the state before the first undo, and the conversation is whole again.
        let UndoResult::Done(r) = f.engine.redo(sid).unwrap() else { panic!() };
        assert_eq!(r.restored, ["a.txt", "b.txt"]);
        assert_eq!((read_work(&f, "a.txt").as_deref(), read_work(&f, "b.txt").as_deref()), (Some("v2"), Some("B")));
        assert_eq!(f.engine.store().revert(sid).unwrap(), None);
        assert_eq!(f.engine.redo(sid).unwrap(), UndoResult::NothingToUndo);

        // Undo turn two again, then continue: the next prompt makes it permanent.
        f.engine.undo(sid).unwrap();
        f.fake.extend(vec![text("three done")]);
        f.say("three");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        assert_eq!(f.engine.store().revert(sid).unwrap(), None);
        let users: Vec<String> = f.msgs().iter().filter(|m| m.kind == "user").map(|m| m.data["text"].as_str().unwrap().to_string()).collect();
        assert_eq!(users, ["one", "three"], "turn two is gone for good");
        let last = f.fake.requests.lock().unwrap().last().unwrap().clone();
        assert!(all_text(&last).contains("one") && all_text(&last).contains("three") && !all_text(&last).contains("two"));
        assert_eq!((read_work(&f, "a.txt").as_deref(), read_work(&f, "b.txt")), (Some("v1"), None), "files stay as the undo left them");
    }

    #[test]
    fn session_diff_shows_everything_since_the_first_step() {
        let f = two_turns();
        let DiffResult::Files(d) = f.engine.session_diff(&f.session.id).unwrap() else { panic!() };
        let s: Vec<_> = d.iter().map(|x| (x.path.as_str(), x.status, x.additions)).collect();
        assert_eq!(s, [("a.txt", crate::snapshot::Status::Added, 1), ("b.txt", crate::snapshot::Status::Added, 1)]);
        assert!(d[0].patch.contains("+v2"));
        f.file("a.txt", "v3 by hand");
        let DiffResult::Files(d) = f.engine.session_diff(&f.session.id).unwrap() else { panic!() };
        assert!(d[0].patch.contains("+v3 by hand"), "edits made outside the agent show up too");
        let empty = fixture(vec![]);
        assert_eq!(empty.engine.session_diff(&empty.session.id).unwrap(), DiffResult::NoSnapshots);
    }

    #[test]
    fn with_snapshots_off_undo_only_rolls_back_the_conversation() {
        let f = fixture_cfg(vec![tool_step(vec![write_call("w1", "a.txt", "v1")]), text("done")], |c| c.snapshots = false);
        f.say("one");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        assert!(f.msgs().iter().all(|m| m.data.get("snapshot").is_none()));
        let UndoResult::Done(r) = f.engine.undo(&f.session.id).unwrap() else { panic!() };
        assert!(!r.files_restored && r.restored.is_empty());
        assert_eq!(read_work(&f, "a.txt").as_deref(), Some("v1"), "files untouched");
        assert!(matches!(f.engine.redo(&f.session.id).unwrap(), UndoResult::Done(_)));
    }

    #[test]
    fn too_many_files_turns_snapshots_off_with_one_notice() {
        let f = fixture_cfg(vec![tool_call("t1", "glob", json!({"pattern":"*"})), text("done")], |c| c.snapshot_max_files = 2);
        for i in 0..4 {
            f.file(&format!("f{i}.txt"), "x");
        }
        f.say("go");
        let ui = TestUi::default();
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        let notices: Vec<_> = ui.log().into_iter().filter(|l| l.contains("snapshots are off")).collect();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(f.msgs().iter().all(|m| m.data.get("snapshot").is_none()));
    }

    #[test]
    fn undo_refuses_while_the_session_is_running() {
        let f = two_turns();
        assert!(f.engine.store().claim(&f.session.id).unwrap());
        assert_eq!(f.engine.undo(&f.session.id).unwrap(), UndoResult::Busy);
        assert_eq!(f.engine.redo(&f.session.id).unwrap(), UndoResult::Busy);
        assert_eq!(read_work(&f, "b.txt").as_deref(), Some("B"));
    }

    #[test]
    fn pruning_keeps_what_recent_sessions_use_and_undo_degrades_gracefully() {
        let f = two_turns();
        let pid = f.session.project_id.clone();
        let dir = f.dir.join("app/data/snapshots").join(&pid).join("manifests");
        assert!(std::fs::read_dir(&dir).unwrap().count() >= 3);
        // A session active within the window protects its snapshots, however old the files are.
        assert_eq!(f.engine.prune_snapshots(&[pid.clone()], Duration::from_secs(3600)), (0, 0));
        // With a zero window every session counts as stale: old snapshots go (the files on disk never do).
        let (manifests, blobs) = f.engine.prune_snapshots(&[pid.clone()], Duration::ZERO);
        assert!(manifests >= 3 && blobs >= 3, "{manifests} {blobs}");
        assert_eq!(read_work(&f, "a.txt").as_deref(), Some("v2"));
        match f.engine.undo(&f.session.id).unwrap() {
            UndoResult::Failed(m) => assert!(m.contains("snapshot data missing"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(read_work(&f, "b.txt").as_deref(), Some("B"), "a failed undo changes nothing");
    }

    fn titles(f: &Fixture) -> Vec<Request> {
        f.fake.title_requests.lock().unwrap().clone()
    }
    fn title_of(f: &Fixture) -> Option<String> {
        f.engine.store().session(&f.session.id).unwrap().unwrap().title
    }

    #[test]
    fn a_new_session_gets_a_title_from_its_first_prompt_once() {
        let f = fixture(vec![text("working on it"), text("again")]);
        f.fake.script_titles(vec![text("Title: \"Production 500 errors.\""), text("Should not be used")]);
        f.say("debug 500 errors in production");
        let ui = TestUi::default();
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        assert_eq!(title_of(&f).as_deref(), Some("Production 500 errors"), "cleaned: label, quotes and period removed");
        assert!(ui.log().contains(&"title:Production 500 errors".to_string()));
        let t = titles(&f);
        assert_eq!(t.len(), 1);
        assert!(t[0].tools.is_empty() && matches!(&t[0].messages[0].content[0], Block::Text(x) if x.contains("debug 500 errors in production")));
        assert_eq!(f.fake.calls(), 1, "title requests are not counted with the conversation's");
        f.say("another question");
        f.run(&TestUi::default());
        assert_eq!(titles(&f).len(), 1, "no second title request");
        assert_eq!(title_of(&f).as_deref(), Some("Production 500 errors"));
        assert!(f.engine.store().usage_totals(&f.session.id).unwrap().1.input >= 10, "the title call's tokens are counted");
    }

    #[test]
    fn a_title_you_set_is_never_replaced_or_even_requested() {
        let f = fixture(vec![text("ok")]);
        f.engine.store().set_title(&f.session.id, "My own title").unwrap();
        f.fake.script_titles(vec![text("Generated")]);
        f.say("hello");
        f.run(&TestUi::default());
        assert_eq!((title_of(&f).as_deref(), titles(&f).len()), (Some("My own title"), 0));
    }

    #[test]
    fn title_failures_never_break_the_run() {
        // A non-retryable error: no title, no retry, the turn is unaffected.
        let f = fixture(vec![text("fine")]);
        f.fake.script_titles(vec![Scripted::Fail(vec![], ProviderError::Auth("no".into()))]);
        f.say("hi");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        assert_eq!((title_of(&f), titles(&f).len()), (None, 1));
        // Connection errors (request provably never ran) are retried up to three times.
        let conn = || Scripted::Fail(vec![], ProviderError::Transport { kind: crate::provider::TransportKind::Connect, message: "refused".into() });
        let f = fixture(vec![text("fine")]);
        f.fake.script_titles(vec![conn(), conn(), text("Third time lucky")]);
        f.say("hi");
        f.run(&TestUi::default());
        assert_eq!(title_of(&f).as_deref(), Some("Third time lucky"));
        assert_eq!(*f.waits.lock().unwrap(), [Duration::from_secs(1), Duration::from_secs(1)]);
        let f = fixture(vec![text("fine")]);
        f.fake.script_titles(vec![conn(), conn(), conn(), conn(), text("never reached")]);
        f.say("hi");
        f.run(&TestUi::default());
        assert_eq!((title_of(&f), titles(&f).len()), (None, 4), "1 try + 3 retries, then give up");
        // A timeout may have reached the provider, so it is not retried.
        let f = fixture(vec![text("fine")]);
        f.fake.script_titles(vec![Scripted::Fail(vec![], ProviderError::Transport { kind: crate::provider::TransportKind::Timeout, message: "t".into() }), text("nope")]);
        f.say("hi");
        f.run(&TestUi::default());
        assert_eq!((title_of(&f), titles(&f).len()), (None, 1));
    }

    #[test]
    fn small_model_is_used_for_titles_and_replies_are_cleaned_of_noise() {
        let f = fixture_cfg(vec![text("ok")], |c| c.small_model = Some("anthropic/claude-small".into()));
        f.fake.script_titles(vec![text("\n  **Dark mode toggle**  \nThis title names the request.")]);
        f.say("add a dark mode toggle");
        f.run(&TestUi::default());
        assert_eq!(titles(&f)[0].model, "claude-small");
        assert_eq!(f.fake.requests.lock().unwrap()[0].model, "test-model");
        assert_eq!(title_of(&f).as_deref(), Some("Dark mode toggle"));
    }

    #[test]
    fn subagent_sessions_are_titled_by_their_task_and_never_request_one() {
        let f = fixture(vec![sub("t1", "general", "do the thing"), text("child answer"), text("parent answer")]);
        f.fake.script_titles(vec![text("Delegation test")]);
        f.say("delegate");
        f.run(&TestUi::default());
        let cid = child_id(&f.fake.requests.lock().unwrap()[2]);
        assert_eq!(f.engine.store().session(&cid).unwrap().unwrap().title.as_deref(), Some("do it"));
        assert_eq!(titles(&f).len(), 1, "only the top-level session asked for a title");
        assert_eq!(title_of(&f).as_deref(), Some("Delegation test"));
    }

    #[test]
    fn forking_by_turns_back() {
        let f = fixture(vec![text("a1"), text("a2"), text("a3")]);
        for p in ["first", "second", "third"] {
            f.say(p);
            f.run(&TestUi::default());
        }
        let users = |sid: &str| -> Vec<String> { f.engine.store().messages(sid).unwrap().iter().filter(|m| m.kind == "user").map(|m| m.data["text"].as_str().unwrap().to_string()).collect() };
        let all = f.engine.fork(&f.session.id, 0).unwrap().unwrap();
        assert_eq!(users(&all.id), ["first", "second", "third"]);
        let one_back = f.engine.fork(&f.session.id, 1).unwrap().unwrap();
        assert_eq!(users(&one_back.id), ["first", "second"], "as it was before the latest prompt");
        assert_eq!(users(&f.engine.fork(&f.session.id, 3).unwrap().unwrap().id), Vec::<String>::new());
        assert_eq!(f.engine.fork(&f.session.id, 4).unwrap(), None);
        // The fork is an independent conversation.
        f.fake.extend(vec![text("fork answer")]);
        f.engine.submit(&one_back.id, "different third", Mode::Steer).unwrap();
        assert_eq!(f.engine.run(&one_back.id, &TestUi::default(), &AtomicBool::new(false), false).unwrap(), RunOutcome::Idle);
        let last = f.fake.requests.lock().unwrap().last().unwrap().clone();
        assert!(all_text(&last).contains("second") && all_text(&last).contains("different third") && !all_text(&last).contains("third\n"));
        assert_eq!(users(&f.session.id), ["first", "second", "third"], "the original is untouched");
    }

    fn demo_skill(f: &Fixture) -> skills::Skill {
        let dir = f.dir.join("skills/release");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.md"), "n").unwrap();
        skills::Skill { id: "release".into(), name: "release".into(), description: "Cut a release: bump, tag, publish".into(), dir, body: "1. Bump the version.\n2. Tag it.".into() }
    }

    #[test]
    fn skills_are_listed_in_the_prompt_and_loadable_with_the_tool() {
        // The fixture must exist before the skill (its dir lives in the fixture), so build the engine twice-free: use a tweak with a temp path.
        let dir = std::env::temp_dir().join(crate::id::new_id("moco-sk-eng"));
        let sk_dir = dir.join("release");
        std::fs::create_dir_all(&sk_dir).unwrap();
        let skill = skills::Skill { id: "release".into(), name: "release".into(), description: "Cut a release: bump, tag, publish".into(), dir: sk_dir, body: "1. Bump the version.\n2. Tag it.".into() };
        let f = fixture_cfg(vec![tool_call("t1", "skill", json!({"id":"release"})), text("following it")], |c| c.skills = vec![skill]);
        f.say("make a release");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert!(reqs[0].tools.iter().any(|t| t.name == "skill"));
        assert!(reqs[0].system.contains("<available_skills>") && reqs[0].system.contains("<id>release</id>") && reqs[0].system.contains("Cut a release: bump, tag, publish"), "{}", reqs[0].system);
        assert!(result_text(&reqs[1]).contains("# Skill: release") && result_text(&reqs[1]).contains("2. Tag it."));
        let _ = std::fs::remove_dir_all(dir);
        let _ = demo_skill;
    }

    #[test]
    fn without_skills_there_is_no_tool_and_no_prompt_section() {
        let f = fixture(vec![text("ok")]);
        f.say("hi");
        f.run(&TestUi::default());
        let reqs = f.fake.requests.lock().unwrap();
        assert!(!reqs[0].tools.iter().any(|t| t.name == "skill") && !reqs[0].system.contains("available_skills") && !reqs[0].system.contains("# Skills"));
    }

    #[test]
    fn explore_does_not_get_skills() {
        let sk = skills::Skill { id: "s".into(), name: "s".into(), description: "d".into(), dir: std::env::temp_dir(), body: "b".into() };
        let f = fixture_cfg(vec![sub("t1", "explore", "look"), text("found"), text("done")], |c| c.skills = vec![sk]);
        f.say("go");
        f.run(&TestUi::default());
        let reqs = f.fake.requests.lock().unwrap();
        assert!(reqs[0].system.contains("available_skills"), "the main agent sees the list");
        assert!(!reqs[1].system.contains("available_skills") && !reqs[1].tools.iter().any(|t| t.name == "skill"), "read-only explore has no skill tool, so no list");
    }

    #[test]
    fn mentions_are_stored_with_the_prompt_and_reach_the_model() {
        let f = fixture(vec![text("seen"), text("again")]);
        f.file("notes.txt", "remember the milk\n");
        let skills_none: Vec<skills::Skill> = vec![];
        let ctx = crate::mentions::Ctx { cwd: &f.dir.join("work"), skills: &skills_none, subagents: &["explore".to_string()], app_dir: f.engine.app_dir(), caps: MediaCaps::default(), mcp: None };
        let (m, notes) = crate::mentions::expand("what does @notes.txt say? ask @explore too", &ctx);
        assert_eq!(notes.len(), 2);
        f.engine.submit_with(&f.session.id, "what does @notes.txt say? ask @explore too", Some(m), Mode::Steer).unwrap();
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        // The file is stored as it was when sent: later edits do not change history.
        std::fs::write(f.dir.join("work/notes.txt"), "changed").unwrap();
        f.say("and now?");
        f.run(&TestUi::default());
        let reqs = f.fake.requests.lock().unwrap();
        let first = all_text(&reqs[1]);
        assert!(first.contains("<file path=\"notes.txt\">\nremember the milk\n</file>") && first.contains("(agent: \"explore\")"), "{first}");
        assert!(!first.contains("changed"), "history keeps the content from submit time");
        assert_eq!(reqs[0].messages.len(), 1, "typed text and attachments are one user message");
        // The title is made from what the user typed, not the attachment.
        assert_eq!(f.engine.store().title_source(&f.session.id).unwrap().unwrap().0, "what does @notes.txt say? ask @explore too");
    }

    #[test]
    fn a_subagent_can_be_run_directly_for_a_command() {
        let f = fixture(vec![text("review done")]);
        let parent = f.engine.store().session(&f.session.id).unwrap().unwrap();
        let r = f
            .engine
            .run_subagent(&parent, SubagentRequest { agent: "general".into(), description: "review".into(), prompt: "review the diff".into(), model: None, session_id: None }, &TestUi::default(), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(r.output, "review done");
        assert_eq!(f.engine.store().session(&r.session_id).unwrap().unwrap().parent_id.as_deref(), Some(f.session.id.as_str()));
    }

    fn tiny_png() -> Vec<u8> {
        crate::media::tests::png(32, 16)
    }
    fn has_image(req: &Request) -> bool {
        req.messages.iter().flat_map(|m| &m.content).any(|b| matches!(b, Block::Image { media_type, .. } if media_type == "image/png"))
    }

    #[test]
    fn an_image_read_by_the_model_reaches_it_and_is_stored_as_a_blob() {
        let f = fixture(vec![tool_call("t1", "read", json!({"path":"shot.png"})), text("a small png")]);
        std::fs::write(f.dir.join("work/shot.png"), tiny_png()).unwrap();
        f.say("what is in shot.png");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert!(has_image(&reqs[1]) && !has_image(&reqs[0]));
        let last = reqs[1].messages.last().unwrap();
        assert!(matches!(&last.content[0], Block::ToolResult { content, .. } if content.starts_with("image file shot.png (image/png, 32x16")));
        assert!(matches!(&last.content[1], Block::Image { .. }), "the image follows the tool result in the same user message");
        // The stored message holds a reference only.
        let stored = f.msgs();
        let state = &stored[1].data["parts"][0]["state"];
        assert_eq!(state["media"][0]["mediaType"], "image/png");
        let hash = state["media"][0]["blob"].as_str().unwrap();
        assert_eq!(blobs::get(f.engine.app_dir(), hash).unwrap(), tiny_png());
        assert!(!stored[1].data.to_string().contains("iVBOR") && !state["metadata"].to_string().contains("_media"));
    }

    #[test]
    fn an_at_mentioned_image_is_sent_with_the_prompt() {
        let f = fixture(vec![text("it is a diagram")]);
        std::fs::write(f.dir.join("work/diagram.png"), tiny_png()).unwrap();
        let work = f.dir.join("work");
        let ctx = crate::mentions::Ctx { cwd: &work, skills: &[], subagents: &[], app_dir: f.engine.app_dir(), caps: MediaCaps::default(), mcp: None };
        let (m, notes) = crate::mentions::expand("explain @diagram.png", &ctx);
        assert!(notes[0].starts_with("attached image diagram.png"));
        f.engine.submit_with(&f.session.id, "explain @diagram.png", Some(m), Mode::Steer).unwrap();
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert!(has_image(&reqs[0]));
        assert_eq!(reqs[0].messages.len(), 1);
        assert!(all_text(&reqs[0]).contains("<attachment path=\"diagram.png\" />"));
    }

    #[test]
    fn a_missing_blob_degrades_gracefully_instead_of_failing_the_request() {
        let f = fixture(vec![text("ok")]);
        f.engine.store().add_message(&f.session.id, "user", &json!({"text":"old","mentions":{"files":[{"path":"gone.png","kind":"media","mediaType":"image/png","blob":"0000000000000000000000000000000000000000000000000000000000000000","size":10,"name":"gone.png"}]}})).unwrap();
        f.say("continue");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert!(!has_image(&reqs[0]) && all_text(&reqs[0]).contains("[attachment gone.png is no longer available]"));
    }

    #[test]
    fn models_without_image_input_are_refused_before_the_api_does_it_for_you() {
        let probe = Engine::new(Store::open_memory().unwrap(), Box::new(FakeProvider::new(vec![])), EngineConfig::new("x", std::env::temp_dir()));
        let (pid, mid) = probe
            .catalog()
            .providers
            .iter()
            .find_map(|(pid, p)| p.models.iter().find(|(_, m)| !m.modalities.input.is_empty() && !m.accepts("image")).map(|(mid, _)| (pid.clone(), mid.clone())))
            .expect("the catalog has a text-only model");
        assert!(!probe.media_caps(&pid, &mid).image);
        assert_eq!(probe.media_caps("unknown", "unknown"), MediaCaps::default());
        let f = fixture_cfg(vec![tool_call("t1", "read", json!({"path":"shot.png"})), text("cannot see it")], |c| c.model = format!("{pid}/{mid}"));
        std::fs::write(f.dir.join("work/shot.png"), tiny_png()).unwrap();
        f.say("look");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert!(!has_image(&reqs[1]));
        assert!(matches!(&reqs[1].messages.last().unwrap().content[0], Block::ToolResult { is_error: true, content, .. } if content.contains("does not accept image input")));
    }

    fn fake_mcp(dir: &std::path::Path) -> Option<Arc<crate::mcp::Manager>> {
        std::process::Command::new("python3").arg("--version").output().ok().filter(|o| o.status.success())?;
        let spec = crate::mcp::ServerSpec {
            name: "fake".into(),
            config: config::McpServerConfig { command: vec!["python3".into(), concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_mcp.py").into()], ..Default::default() },
            cwd: dir.to_path_buf(),
        };
        let m = crate::mcp::Manager::start(vec![spec], &config::McpTimeouts::default(), &std::env::temp_dir());
        assert!(m.wait_ready(Duration::from_secs(20)));
        Some(m)
    }

    #[test]
    fn mcp_tools_join_the_builtin_ones_per_request() {
        let dir = std::env::temp_dir().join(crate::id::new_id("moco-eng-mcp"));
        std::fs::create_dir_all(&dir).unwrap();
        let Some(mcp) = fake_mcp(&dir) else { return eprintln!("skipped: python3 not available") };
        let f = fixture_cfg(
            vec![
                tool_call("t1", "fake_echo", json!({"text":"from the model"})),
                tool_call("t2", "fake_add_tool", json!({})),
                text("now there is one more tool"),
                tool_call("t3", "fake_added", json!({})),
                text("used it"),
            ],
            |c| c.mcp = Some(mcp.clone()),
        );
        f.say("use the mcp tools");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        let names = |r: &Request| r.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
        assert!(names(&reqs[0]).contains(&"fake_echo".to_string()) && names(&reqs[0]).contains(&"read".to_string()) && names(&reqs[0]).contains(&"mcp-resource".to_string()));
        let echo = reqs[0].tools.iter().find(|t| t.name == "fake_echo").unwrap();
        assert_eq!((echo.description.as_str(), echo.schema["required"].clone()), ("Echo the text back", json!(["text"])));
        assert_eq!(result_text(&reqs[1]), "echo: from the model");
        assert!(!names(&reqs[0]).contains(&"fake_added".to_string()));
        // The server announced a new tool during step 2; the very next request already lists it.
        let wait_end = std::time::Instant::now() + Duration::from_secs(5);
        while !mcp.tools().iter().any(|t| t.name() == "fake_added") && std::time::Instant::now() < wait_end {
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(reqs);
        f.fake.extend(vec![text("x")]);
        f.say("again");
        f.run(&TestUi::default());
        let reqs = f.fake.requests.lock().unwrap();
        assert!(reqs.last().unwrap().tools.iter().any(|t| t.name == "fake_added"));
        mcp.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rules_apply_to_mcp_tools_by_name() {
        let dir = std::env::temp_dir().join(crate::id::new_id("moco-eng-mcp2"));
        std::fs::create_dir_all(&dir).unwrap();
        let Some(mcp) = fake_mcp(&dir) else { return eprintln!("skipped: python3 not available") };
        let cfg: config::Config = serde_json::from_value(json!({"permissions":[{"action":"fake_*","resource":"*","effect":"ask"}]})).unwrap();
        let f = fixture_cfg(vec![tool_call("t1", "fake_echo", json!({"text":"x"})), text("asked first"), sub("t2", "explore", "look"), tool_call("c1", "fake_echo", json!({"text":"from explore"})), text("explore done"), text("finished")], |c| {
            c.mcp = Some(mcp.clone());
            c.agents = config::resolve_agents_in(&cfg, crate::permission::PermissionMode::Auto);
        });
        f.say("go");
        let ui = TestUi::replying(vec![Reply::Once]);
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        assert_eq!(ui.asked.lock().unwrap().as_slice(), ["fake_echo *"], "a wildcard rule on the tool name made it ask");
        // Your rules come last, so they also override a built-in agent's blanket deny: explore may now use these tools, with asking.
        f.say("now delegate");
        let ui = TestUi::replying(vec![Reply::Once]);
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        assert_eq!(ui.asked.lock().unwrap().as_slice(), ["fake_echo *"]);
        mcp.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_only_agents_do_not_get_mcp_tools_unless_you_say_so() {
        let dir = std::env::temp_dir().join(crate::id::new_id("moco-eng-mcp3"));
        std::fs::create_dir_all(&dir).unwrap();
        let Some(mcp) = fake_mcp(&dir) else { return eprintln!("skipped: python3 not available") };
        let f = fixture_cfg(vec![sub("t1", "explore", "look"), text("explore done"), text("finished")], |c| c.mcp = Some(mcp.clone()));
        f.say("delegate");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert!(reqs[0].tools.iter().any(|t| t.name == "fake_echo"));
        let explore_req = reqs.iter().find(|r| r.system.contains("file search specialist")).expect("explore ran");
        assert!(!explore_req.tools.iter().any(|t| t.name.starts_with("fake_") || t.name == "mcp-resource"), "explore denies everything it does not list");
        mcp.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
        let end = std::time::Instant::now() + Duration::from_secs(15);
        while !f() {
            assert!(std::time::Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A `sleep` duration that appears in no source text or invoking command line, so `pgrep -f` can find exactly our processes.
    fn uniq(k: u32) -> u32 {
        20_000 + std::process::id() % 9_000 + k * 10_000
    }

    fn alive(n: u32) -> bool {
        std::process::Command::new("pgrep").args(["-f", &format!("sleep {n}")]).output().map(|o| o.status.success()).unwrap_or(false)
    }

    fn bg_shell(id: &str, command: &str) -> Scripted {
        tool_call(id, "shell", json!({"command": command, "background": true}))
    }

    #[test]
    fn a_background_command_reports_back_after_the_turn_that_started_it() {
        let f = fixture(vec![bg_shell("t1", "sleep 0.3; echo done-bg; exit 3"), text("it is running"), text("noted the result")]);
        f.say("start it");
        let notes = Arc::new(Mutex::new(vec![]));
        let n = notes.clone();
        f.engine.set_notifier(Arc::new(move |l| n.lock().unwrap().push(l.to_string())));
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let started = result_text(&f.fake.requests.lock().unwrap()[1]);
        assert!(started.starts_with("Command moved to the background (shell ID: shl_") && started.contains("Output is streaming to: "), "{started}");
        assert_eq!(f.engine.running_jobs(), 1);
        wait_until("the command to finish", || f.engine.store().pending_inbox(&f.session.id).unwrap() > 0);
        // The result is waiting for the next run; nobody had to prompt for it.
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        let t = all_text(&reqs[2]);
        assert!(t.contains("Background command finished (shell ID: shl_") && t.contains("exited (code 3)") && t.contains("Command: sleep 0.3; echo done-bg; exit 3") && t.contains("done-bg"), "{t}");
        assert_eq!(f.engine.background_jobs(None)[0].status, "exited (code 3)");
        assert_eq!(notes.lock().unwrap().len(), 1);
        assert!(notes.lock().unwrap()[0].contains("exited (code 3)"));
    }

    #[test]
    fn a_command_that_finishes_during_a_turn_is_seen_at_the_next_step() {
        let f = fixture(vec![bg_shell("t1", "sleep 0.2; echo from-bg"), tool_call("t2", "shell", json!({"command": "sleep 0.8"})), text("done")]);
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert_eq!(reqs.len(), 3);
        let last = reqs[2].messages.last().unwrap();
        assert!(matches!(&last.content[0], Block::ToolResult { .. }));
        assert!(matches!(&last.content[1], Block::Text(t) if t.contains("Background command finished") && t.contains("from-bg")), "{:?}", last.content);
        assert_eq!(f.engine.store().pending_inbox(&f.session.id).unwrap(), 0);
    }

    #[test]
    fn jobs_can_be_listed_read_and_stopped_and_stopping_leaves_no_process() {
        let n = uniq(0);
        let f = fixture(vec![bg_shell("t1", &format!("echo started-marker; sleep {n}")), tool_call("t2", "jobs", json!({"action":"list"})), text("ok")]);
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let list = result_text(&f.fake.requests.lock().unwrap()[2]);
        assert!(list.contains("shell  running") && list.contains(&format!("echo started-marker; sleep {n}")), "{list}");
        let id = f.engine.background_jobs(None)[0].id.clone();
        wait_until("output", || f.engine.job_output(&id, 10).unwrap().contains("started-marker"));
        assert!(f.engine.job_output(&id, 10).unwrap().starts_with(&format!("[{id}: running]")));
        assert!(alive(n));
        assert_eq!(f.engine.kill_job(&id).unwrap(), format!("Stopped {id}."));
        assert_eq!(f.engine.background_jobs(None)[0].status, "stopped");
        wait_until("the process to be gone", || !alive(n));
        assert_eq!(f.engine.store().pending_inbox(&f.session.id).unwrap(), 0, "a job you stopped yourself is not announced");
        assert!(f.engine.kill_job("shl_nope").is_err());
        assert!(f.engine.kill_job(&id).unwrap().contains("is not running"));
    }

    #[test]
    fn background_commands_can_have_a_timeout() {
        let f = fixture(vec![tool_call("t1", "shell", json!({"command": "sleep 1235", "background": true, "timeout": 200})), text("ok"), text("saw it")]);
        f.say("go");
        f.run(&TestUi::default());
        wait_until("the timeout", || f.engine.store().pending_inbox(&f.session.id).unwrap() > 0);
        f.run(&TestUi::default());
        let t = all_text(&f.fake.requests.lock().unwrap()[2]);
        assert!(t.contains("timed out") && t.contains("[command timed out after 200 ms and was killed]"), "{t}");
    }

    #[test]
    fn a_background_subagent_runs_unattended_and_reports_its_result() {
        let cfg: config::Config = serde_json::from_value(json!({"permissions":[{"action":"shell","resource":"*","effect":"ask"}]})).unwrap();
        let f = fixture_cfg(
            vec![
                tool_call("t1", "subagent", json!({"agent":"general","description":"background task","prompt":"BG-TASK-1 do the work","background":true})),
                text("started it"),
                text("the parent saw the report"),
            ],
            |c| c.agents = config::resolve_agents_in(&cfg, crate::permission::PermissionMode::Auto),
        );
        f.fake.script_route("BG-TASK-1", vec![tool_call("c1", "shell", json!({"command":"echo hi"})), text("finished, but could not run the command")]);
        f.say("delegate in the background");
        let ui = TestUi::default();
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        let started = result_text(&f.fake.requests.lock().unwrap()[1]);
        assert!(started.starts_with("Subagent started in the background (sessionID: ses_"), "{started}");
        // The report reaches the parent either inside the turn that started the subagent (if it finishes quickly, which
        // a scripted one does) or with the next run; both are right, so wait for the job and run again only if needed.
        wait_until("the subagent to finish", || f.engine.running_jobs() == 0);
        assert!(ui.asked.lock().unwrap().is_empty(), "nobody was asked: the subagent had no one to ask");
        // It was refused with a reason the model could act on.
        let routed = f.fake.routed_requests.lock().unwrap().clone();
        let refusal = result_text(&routed[1]);
        assert!(refusal.contains("nobody is present to give it"), "{refusal}");
        if f.engine.store().pending_inbox(&f.session.id).unwrap() > 0 {
            f.run(&TestUi::default());
        }
        let t = all_text(&f.fake.requests.lock().unwrap()[2]);
        assert!(t.contains("Background subagent finished (sessionID: ses_") && t.contains("task: background task") && t.contains("finished, but could not run the command"), "{t}");
        assert_eq!(f.engine.background_jobs(None)[0].status, "finished");
    }

    #[test]
    fn shutdown_stops_running_jobs_of_both_kinds() {
        let (n1, n2) = (uniq(1), uniq(2));
        let f = fixture(vec![
            bg_shell("t1", &format!("sleep {n1}")),
            tool_call("t2", "subagent", json!({"agent":"general","description":"stuck task","prompt":"BG-TASK-2","background":true})),
            text("both started"),
        ]);
        f.fake.script_route("BG-TASK-2", vec![tool_call("c1", "shell", json!({"command": format!("sleep {n2}")}))]);
        f.say("go");
        f.run(&TestUi::default());
        wait_until("both to be running", || f.engine.running_jobs() == 2);
        wait_until("both commands to exist", || alive(n1) && alive(n2));
        assert_eq!(f.engine.kill_all_jobs(), 2);
        assert_eq!(f.engine.running_jobs(), 0);
        wait_until("processes gone", || !alive(n1) && !alive(n2));
        let statuses: Vec<String> = f.engine.background_jobs(None).iter().map(|j| j.status.clone()).collect();
        assert_eq!(statuses, ["stopped", "stopped"]);
        assert_eq!(f.engine.store().pending_inbox(&f.session.id).unwrap(), 0);
    }

    #[test]
    fn background_work_needs_a_shared_engine() {
        let dir = std::env::temp_dir().join(crate::id::new_id("moco-eng-unshared"));
        std::fs::create_dir_all(dir.join("work")).unwrap();
        let fake = Arc::new(FakeProvider::new(vec![bg_shell("t1", "true"), text("ok")]));
        let store = Store::open_memory().unwrap();
        let ses = store.create_session(dir.join("work").to_str().unwrap(), "build").unwrap();
        let mut cfg = EngineConfig::new("m", dir.join("app"));
        cfg.agents = crate::agent::builtin_for(crate::permission::PermissionMode::Auto);
        let engine = Engine::new(store, Box::new(Shared(fake.clone())), cfg); // no .share()
        engine.submit(&ses.id, "go", Mode::Steer).unwrap();
        engine.run(&ses.id, &TestUi::default(), &AtomicBool::new(false), false).unwrap();
        assert!(result_text(&fake.requests.lock().unwrap()[1]).contains("background jobs are not available"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_users_own_shell_command_is_run_and_recorded() {
        let f = fixture(vec![text("saw it")]);
        f.file("a.txt", "x");
        let out = f.engine.run_user_command(&f.session.id, "ls; echo oops >&2; exit 2", &AtomicBool::new(false)).unwrap();
        assert!(out.contains("a.txt") && out.contains("oops") && out.contains("[exit code 2]"), "{out}");
        f.say("what did I just run?");
        f.run(&TestUi::default());
        let t = all_text(&f.fake.requests.lock().unwrap()[0]);
        assert!(t.contains("The user ran this command themselves:\n$ ls; echo oops >&2; exit 2") && t.contains("a.txt"), "{t}");
        let (flag, _) = (AtomicBool::new(true), 0);
        assert_eq!(f.engine.run_user_command(&f.session.id, "sleep 5", &flag).unwrap_err(), "interrupted");
    }

    #[test]
    fn provider_replay_data_survives_the_store_and_returns_with_the_next_request() {
        use crate::provider::{gemini, openai_responses};
        let reasoning = json!({"type":"reasoning","id":"rs_1","encrypted_content":"ENC","summary":[]});
        let fc = json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{\"path\":\"a.txt\"}","status":"completed"});
        let sig = json!({"call_id":"call_1","thoughtSignature":"SIG"});
        let f = fixture(vec![
            Scripted::Events(vec![
                Event::Native { provider: openai_responses::PROVIDER.into(), data: reasoning.clone() },
                Event::TextDelta("Reading it.".into()),
                Event::Native { provider: openai_responses::PROVIDER.into(), data: fc.clone() },
                Event::Native { provider: gemini::PROVIDER.into(), data: sig.clone() },
                Event::ToolInputStart { id: "call_1".into(), name: "read".into() },
                Event::ToolCall { id: "call_1".into(), name: "read".into(), input: json!({"path":"a.txt"}) },
                Event::Finish { reason: FinishReason::ToolUse, usage: Usage::default() },
            ]),
            text("done"),
        ]);
        f.file("a.txt", "hello\n");
        f.say("read it");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        // Stored in order, next to the visible parts.
        let parts = f.msgs()[1].data["parts"].as_array().unwrap().clone();
        let kinds: Vec<String> = parts.iter().map(|p| format!("{}:{}", p["type"].as_str().unwrap(), p["provider"].as_str().unwrap_or(p["name"].as_str().unwrap_or("-")))).collect();
        assert_eq!(kinds, ["native:openai-responses", "text:-", "native:openai-responses", "native:gemini", "tool:read"]);
        // The second request carries them back; each adapter replays only its own.
        let reqs = f.fake.requests.lock().unwrap();
        let body = openai_responses::build_body(&reqs[1]);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[1], reasoning, "the reasoning item, verbatim and first");
        assert_eq!(input[2], json!({"role":"assistant","content":[{"type":"output_text","text":"Reading it."}]}));
        assert_eq!(input[3], fc, "the original function_call item, with its fc_ id");
        assert_eq!(input[4]["type"], "function_call_output");
        let g = gemini::build_body(&reqs[1]);
        assert_eq!(g["contents"][1]["parts"][1]["thoughtSignature"], "SIG");
        assert!(!g.to_string().contains("ENC"), "another provider's encrypted reasoning is never sent to Gemini");
        let a = crate::provider::anthropic::build_body(&reqs[1], false).to_string();
        assert!(!a.contains("ENC") && !a.contains("SIG") && !a.contains("native"), "nor to Anthropic");
        assert!(!crate::provider::openai_chat::build_body(&reqs[1], false).to_string().contains("ENC"));
    }
}
