//! Session engine: the step loop (docs/03-agent-loop.md).
//!
//! `run` claims the session, then repeats: deliver inbox items allowed at this boundary, run one step
//! (one LLM call, tools executing concurrently as their calls complete), continue while the step
//! produced tool calls. History is reloaded from the store on every step.
use crate::compat::*;
use crate::agent::{self, Agent};
use crate::snapshot::{FileDiff, Snapshots};
use crate::{blobs, catalog, classifier, compaction, config, instructions, skills};
use crate::chat;
use crate::log::LogErr;
use crate::permission::{self, Effect, PermissionMode, Rule};
use crate::provider::{retry, Block, ChatMessage, Event, FinishReason, Provider, ProviderError, Request, Role, ToolDef, Usage};
use crate::store::{Mode, Session, Store, StoreError};
use crate::tools::{self, project_root_of, Host, MediaCaps, PermReq, Permitter, Question, SubagentRequest, SubagentResult, Todo, Tool, ToolCtx, ToolError, ToolOutput};
use serde_json::{json, Value};
mod classify;
pub use classify::Evaluation;
mod jobs;
mod limits;
mod compact;
mod permit;
use permit::EnginePermitter;
mod step;
mod suggest;
mod title;
mod undo;

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
    ToolStarted { id: &'a str, name: &'a str, input: &'a Value },
    ToolFinished { name: &'a str, ok: bool, text: &'a str },
    Retry { attempt: u32, wait: Duration, error: &'a str },
    /// After each step: this step's tokens and cost, session total, and how full the context is.
    StepUsage { usage: Usage, cost: Option<f64>, session_cost: f64, context_tokens: u64, context_limit: Option<u64> },
    /// The permission classifier was called (`classifier.rs`): what the call cost, the session total (which includes
    /// it) and the part of that total spent on classifying.
    ClassifierUsage { cost: f64, session_cost: f64, classifier_cost: f64 },
    /// The classifier let a shell command through without asking. `call_id` is the tool call it belongs to (the id of
    /// the matching `ToolStarted`), `command` is one line, `risk` is what the model judged it to be (or
    /// `allowed earlier` for a repeat), `reason` may be empty.
    AutoAllowed { call_id: &'a str, command: &'a str, risk: &'a str, reason: &'a str },
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
    /// A spending limit was reached (`what` says which and how far over). The default asks through `ask`
    /// (action `limit`): allow once = continue, always = continue without the limit, reject = stop.
    fn ask_limit(&self, what: &str) -> LimitChoice {
        match self.ask(&PermissionPrompt { action: "limit", resources: &[what.to_string()], save: &[] }) {
            Reply::Once => LimitChoice::Continue,
            Reply::Always => LimitChoice::ContinueUnlimited,
            Reply::Reject(_) => LimitChoice::Stop,
        }
    }
}

/// The user's answer when a spending limit is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitChoice {
    /// Go on until the same amount has been spent again.
    Continue,
    /// Go on and stop checking this limit (for this run, or for the rest of the session's life in this process).
    ContinueUnlimited,
    Stop,
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
    fn ask_limit(&self, what: &str) -> LimitChoice {
        self.parent.ask_limit(what)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Idle,
    Interrupted,
    /// A tool call was declined without feedback.
    Declined,
    Failed(String),
    /// A spending limit (`limits` config, `--max-cost`) stopped the run; `what` says which one and how far over.
    LimitReached { what: String },
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
    /// Run once per step after the tool batch, on the files the step changed (see `check.rs`).
    pub checks: crate::check::Checks,
    /// The `websearch` tool is offered when this is set (see `tools/websearch.rs`).
    pub websearch: Option<tools::websearch::Settings>,
    /// Default reasoning effort (config `effort`); a session's or agent's own choice wins.
    pub effort: Option<String>,
    /// Spending guardrails (config `limits`).
    pub limits: config::Limits,
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
            checks: Default::default(),
            websearch: None,
            effort: None,
            limits: Default::default(),
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

/// Lock a mutex, ignoring poisoning: the data stays usable after a panic on another thread.
pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Releases the session claim when dropped, so a panic in a run cannot leave the session claimed.
struct ClaimGuard<'a> {
    engine: &'a Engine,
    sid: &'a str,
    armed: bool,
}

impl ClaimGuard<'_> {
    /// Release now and report the store error, if any.
    fn release(mut self) -> Result<(), StoreError> {
        self.armed = false;
        self.engine.store().release(self.sid)
    }
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.engine.store().release(self.sid);
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
    /// Checks that failed the last time they ran, per session and check (a later pass is reported once).
    check_failing: Mutex<HashSet<String>>,
    /// Sessions a title was already attempted for in this process.
    title_tried: Mutex<HashSet<String>>,
    /// Commands the classifier allowed in this process (per session and directory); only allows are remembered.
    classified: Mutex<HashSet<String>>,
    /// Classifier models already reported as having no price (once each per process).
    unpriced_noted: Mutex<HashSet<String>>,
    /// Store-failure notices already shown in this process, by key (see `notice_once`).
    notified: Mutex<HashSet<String>>,
    /// Spend baselines for the cost limits (see `limits.rs`).
    limit_state: limits::State,
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
    /// Files that changed but are not covered by the snapshot (too large or unreadable), so were not restored.
    pub skipped: Vec<crate::snapshot::Skipped>,
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

/// Log a failed operation and show `"{what}: {error}"` as a notice the first time `key` fails (later failures are
/// only logged). Returns the value on success.
pub(crate) fn notice_once<T, E: std::fmt::Display>(seen: &Mutex<HashSet<String>>, ui: &dyn Ui, key: &str, what: &str, r: std::result::Result<T, E>) -> Option<T> {
    match r {
        Ok(v) => Some(v),
        Err(e) => {
            crate::log_warn!("store", "{what}: {e}");
            if lock(seen).insert(key.to_string()) {
                ui.event(&UiEvent::Notice(&format!("{what}: {e}")));
            }
            None
        }
    }
}

fn add_usage(a: Usage, b: Usage) -> Usage {
    Usage { input: a.input + b.input, output: a.output + b.output, cache_read: a.cache_read + b.cache_read, cache_write: a.cache_write + b.cache_write }
}

impl Engine {
    /// Single provider answering every model ref.
    pub fn new(store: Store, provider: Box<dyn Provider>, cfg: EngineConfig) -> Self {
        Engine { store: Mutex::new(store), providers: HashMap::new(), fallback: Some(provider), tools: tools::all(subagent_list(&cfg), cfg.skills.clone(), cfg.websearch.clone()).into_iter().map(Arc::from).collect(), cfg, auto_failed: Mutex::new(HashSet::new()), prompt_lock: Mutex::new(()), snapshot_off: Mutex::new(HashSet::new()), check_failing: Mutex::new(HashSet::new()), title_tried: Mutex::new(HashSet::new()), classified: Mutex::new(HashSet::new()), unpriced_noted: Mutex::new(HashSet::new()), notified: Mutex::new(HashSet::new()), me: Mutex::new(std::sync::Weak::new()), jobs: jobs::Jobs::default(), notifier: Mutex::new(None), limit_state: Default::default() }
    }

    /// One client per provider id; a model ref's provider part picks the client.
    pub fn with_providers(store: Store, providers: HashMap<String, Box<dyn Provider>>, cfg: EngineConfig) -> Self {
        Engine { store: Mutex::new(store), providers, fallback: None, tools: tools::all(subagent_list(&cfg), cfg.skills.clone(), cfg.websearch.clone()).into_iter().map(Arc::from).collect(), cfg, auto_failed: Mutex::new(HashSet::new()), prompt_lock: Mutex::new(()), snapshot_off: Mutex::new(HashSet::new()), check_failing: Mutex::new(HashSet::new()), title_tried: Mutex::new(HashSet::new()), classified: Mutex::new(HashSet::new()), unpriced_noted: Mutex::new(HashSet::new()), notified: Mutex::new(HashSet::new()), me: Mutex::new(std::sync::Weak::new()), jobs: jobs::Jobs::default(), notifier: Mutex::new(None), limit_state: Default::default() }
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
    /// How to ask `model` for a short plain-text answer (a title, a verdict): the reasoning effort to request and the
    /// output budget. Thinking tokens count against `max_tokens`, and some models (Haiku 5.5 and the Fable and Opus 5
    /// families) cannot switch thinking off, so a budget sized for the answer alone comes back empty or cut off in the
    /// middle of the answer. Where thinking can be switched off it is; where it cannot, the lowest effort is asked for and
    /// the budget gets room to think.
    pub(crate) fn short_answer_shape(&self, provider_id: &str, model_id: &str, answer_tokens: u32) -> (Option<String>, u32) {
        let levels = self.effort_levels(provider_id, model_id);
        let has = |name: &str| levels.iter().any(|l| l == name);
        if has("none") {
            (Some("none".into()), answer_tokens)
        } else if has("minimal") {
            (Some("minimal".into()), answer_tokens + THINKING_HEADROOM)
        } else if has("low") {
            (Some("low".into()), answer_tokens + THINKING_HEADROOM)
        } else {
            (None, answer_tokens)
        }
    }

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

    /// The session store. Poison-tolerant: a panic on another thread must not take the UI down with it.
    pub fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        lock(&self.store)
    }

    /// Log a failed store write and tell the user once per process per `key` (not on every step). Returns the value.
    pub(crate) fn store_warn<T, E: std::fmt::Display>(&self, ui: &dyn Ui, key: &str, what: &str, r: std::result::Result<T, E>) -> Option<T> {
        notice_once(&self.notified, ui, key, what, r)
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

    /// Lines for `/permissions`: what the classifier allowed without asking in this session (and its subagents), and
    /// the "always allow" rules saved for the project.
    pub fn permissions_report(&self, sid: &str) -> Result<Vec<String>, StoreError> {
        let (session, auto) = {
            let st = self.store();
            match st.session(sid)? {
                Some(session) => (session, st.auto_allows(sid)?),
                // Deleted or archived from another process, or a stale id after /resume: say so, don't take the UI down.
                None => return Ok(vec![format!("session {sid} was not found (it may have been deleted from another moco process)")]),
            }
        };
        let saved = self.store().saved_permissions(&session.project_id)?;
        let mode = self.permission_mode_of(&session);
        let mut out = vec![format!(
            "permission mode: {}; the command classifier is {}",
            mode.name(),
            match (&self.cfg.classifier, mode) {
                (None, _) => "off (permission_classifier.enabled is false)".to_string(),
                (Some(_), PermissionMode::Manual) => "idle (it only works in auto mode)".to_string(),
                (Some(s), _) => format!("on ({})", s.model.clone().unwrap_or_else(|| self.small_model_ref(&session))),
            }
        )];
        out.push(format!("allowed automatically in this session: {}", auto.len()));
        for a in auto.iter().rev().take(50).rev() {
            let when = crate::log::stamp(a.at).replace('T', " ");
            out.push(format!("  {} {:<14} {}{}", &when[..19], a.risk, classifier::sanitize(&a.command, 100), if a.reason.is_empty() { String::new() } else { format!("  ({})", a.reason) }));
        }
        if auto.len() > 50 {
            out.push(format!("  ... {} older not shown", auto.len() - 50));
        }
        out.extend(crate::saved_permissions::listing(&saved, "/permissions"));
        Ok(out)
    }

    /// `/permissions remove <n> | remove <action> <pattern> | clear`: forget saved "always allow" rules of the
    /// session's project. Takes effect at the next permission check, also in a step that is running.
    pub fn manage_saved_permissions(&self, sid: &str, args: &str) -> Result<Vec<String>, crate::saved_permissions::Error> {
        let st = self.store();
        let Some(session) = st.session(sid)? else {
            return Err(crate::saved_permissions::Error::Usage(format!("session {sid} was not found (it may have been deleted from another moco process)")));
        };
        crate::saved_permissions::manage(&st, &session.project_id, "/permissions", args)
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
            crate::log_info!("engine", "run session={session_id} not started: another run holds the claim");
            return Ok(RunOutcome::Busy);
        }
        let claim = ClaimGuard { engine: self, sid: session_id, armed: true };
        crate::log_info!("engine", "run start session={session_id} resume={resume}");
        let run_started = std::time::Instant::now();
        // Sending a new prompt after /undo makes the undo permanent.
        match self.commit_pending(session_id) {
            Ok(0) => {}
            Ok(n) => crate::log_info!("engine", "session={session_id} undo made permanent: {n} message(s) deleted"),
            Err(e) => {
                crate::log_error!("engine", "session={session_id} could not commit the pending undo: {e}");
                claim.release()?;
                return Err(e);
            }
        }
        self.auto_failed.lock().unwrap().remove(session_id);
        // An untitled session gets a title from a small model call that runs alongside the first turn.
        let job = self.title_job(session_id);
        // The title never holds the run back: once the turn is over it gets `TITLE_GRACE` to finish, then it is
        // cancelled (the provider adapters honor that at once, also while waiting for response headers).
        let (title_cancel, title_done, drained) = (AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false));
        let outcome = std::thread::scope(|s| {
            if let Some((session, text)) = &job {
                let (title_cancel, title_done) = (&title_cancel, &title_done);
                s.spawn(move || {
                    self.generate_title(session, text, ui, title_cancel);
                    title_done.store(true, Ordering::Relaxed);
                });
                let drained = &drained;
                s.spawn(move || {
                    let mut drained_at = None;
                    while !title_done.load(Ordering::Relaxed) {
                        if drained.load(Ordering::Relaxed) {
                            drained_at.get_or_insert_with(std::time::Instant::now);
                        }
                        if cancel.load(Ordering::Relaxed) || drained_at.is_some_and(|t| t.elapsed() >= TITLE_GRACE) {
                            title_cancel.store(true, Ordering::Relaxed);
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                });
            }
            let outcome = self.drain(session_id, ui, cancel, resume);
            drained.store(true, Ordering::Relaxed);
            outcome
        });
        claim.release()?;
        // A session deleted from another process mid-run is a failed run, not a crash.
        let outcome = match outcome {
            Err(StoreError::SessionGone) => Ok(RunOutcome::Failed("session no longer exists".into())),
            o => o,
        };
        let ms = run_started.elapsed().as_millis();
        match &outcome {
            Ok(RunOutcome::Failed(why)) => crate::log_warn!("engine", "run end session={session_id} outcome=failed ms={ms}: {why}"),
            Ok(o) => crate::log_info!("engine", "run end session={session_id} outcome={o:?} ms={ms}"),
            Err(e) => crate::log_error!("engine", "run end session={session_id} store error ms={ms}: {e}"),
        }
        outcome
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
            if let Some(m) = req.model.as_ref().or(parent.model.as_ref()) { st.set_model(&child.id, Some(m)).map_err(store_err)? }
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
            RunOutcome::LimitReached { what } => return Err(ToolError::fail(format!("{}{what}", limits::SUBAGENT_STOPPED))),
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
}

/// The engine services a tool can use: todos, questions and subagents of the calling session.
struct EngineHost<'a> {
    engine: &'a Engine,
    session: &'a Session,
    ui: &'a dyn Ui,
    cancel: &'a AtomicBool,
}

const MAX_SUBAGENT_DEPTH: usize = 3;
/// How long a title request may keep running after the turn it accompanied has ended.
const TITLE_GRACE: Duration = Duration::from_secs(3);

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
        // Over the cap is a tool error the model can react to, not a job that starts and fails.
        eng.check_background_slot(true)?;
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

/// Output tokens a model that thinks whatever the request says may spend before a short answer (`short_answer_shape`).
const THINKING_HEADROOM: u32 = 2_000;

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
            UiEvent::AutoAllowed { command, risk, reason, .. } => format!("auto:{risk}:{command}:{reason}"),
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
    fn a_panic_in_a_run_still_releases_the_claim() {
        let f = fixture(vec![text("boom")]);
        f.say("hi");
        let ui = TestUi::default();
        *ui.hook.lock().unwrap() = Some(Box::new(|l| {
            if l == "text:boom" {
                panic!("simulated engine panic");
            }
        }));
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.engine.run(&f.session.id, &ui, &AtomicBool::new(false), false)));
        assert!(r.is_err(), "the panic propagates to the caller");
        assert!(f.engine.store().claim(&f.session.id).unwrap(), "claim released by the guard");
    }

    #[test]
    fn a_session_deleted_mid_run_fails_the_run() {
        let f = fixture(vec![tool_call("t1", "read", json!({"file_path":"a.txt"})), text("never reached")]);
        f.file("a.txt", "hi");
        f.say("go");
        let ui = TestUi::default();
        let (engine, sid) = (f.engine.clone(), f.session.id.clone());
        *ui.hook.lock().unwrap() = Some(Box::new(move |l| {
            if l.starts_with("start:") {
                engine.store().delete_session_for_test(&sid);
            }
        }));
        let r = f.engine.run(&f.session.id, &ui, &AtomicBool::new(false), false).unwrap();
        assert_eq!(r, RunOutcome::Failed("session no longer exists".into()));
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
        assert!(req.system.contains(&format!("- Platform: {}\n", crate::sysinfo::describe())), "the request names this machine");
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

    /// Issue #6 through the whole engine: a model that sends one `edit` per hunk for the same file in one step gets
    /// every hunk applied, however the threads are scheduled.
    #[test]
    fn parallel_edits_to_one_file_in_one_step_all_land() {
        let n = 10;
        let mut events: Vec<Event> = (0..n).map(|i| Event::ToolCall { id: format!("e{i}"), name: "edit".into(), input: json!({"path":"a.txt","oldString":format!("slot{i}=old"),"newString":format!("slot{i}=new")}) }).collect();
        events.push(Event::Finish { reason: FinishReason::ToolUse, usage: Usage::default() });
        let f = fixture(vec![Scripted::Events(events), text("done")]);
        f.file("a.txt", &(0..n).map(|i| format!("slot{i}=old\n")).collect::<String>());
        f.say("rename every slot");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let after = std::fs::read_to_string(f.dir.join("work/a.txt")).unwrap();
        assert_eq!(after.matches("=new").count(), n, "every edit landed:\n{after}");
        let reqs = f.fake.requests.lock().unwrap();
        let results = last_user_blocks(&reqs[1]);
        assert_eq!(results.len(), n);
        assert!(results.iter().all(|b| matches!(b, Block::ToolResult { is_error: false, .. })), "{results:?}");
    }

    /// Issue #26: a step with several edits runs each matching check once, afterwards, and the model sees the
    /// diagnostics in the last matching tool's result.
    fn two_edits_and_a_note(f: &Fixture) -> Vec<Scripted> {
        f.file("a.txt", "one\ntwo\n");
        f.file("b.txt", "alpha\n");
        f.file("c.md", "doc\n");
        let ev = |id: &str, path: &str, old: &str, new: &str| Event::ToolCall { id: id.into(), name: "edit".into(), input: json!({"path": path, "oldString": old, "newString": new}) };
        vec![
            Scripted::Events(vec![ev("e1", "a.txt", "one", "1"), ev("e2", "a.txt", "two", "2"), ev("e3", "b.txt", "alpha", "beta"), ev("e4", "c.md", "doc", "DOC"), Event::Finish { reason: FinishReason::ToolUse, usage: Usage::default() }]),
            text("done"),
        ]
    }

    fn tool_outputs(f: &Fixture) -> Vec<String> {
        let m = f.msgs();
        let a = m.iter().find(|m| m.kind == "assistant").unwrap();
        a.data["parts"].as_array().unwrap().iter().filter(|p| p["type"] == "tool").map(|p| p["state"]["output"].as_str().unwrap_or("").to_string()).collect()
    }

    fn checks_fixture(allowed: bool, tweak: impl FnOnce(&mut EngineConfig)) -> (Fixture, PathBuf) {
        let probe = std::env::temp_dir().join(crate::id::new_id("chk-probe"));
        std::fs::create_dir_all(&probe).unwrap();
        let p = probe.clone();
        let f = fixture_cfg(vec![], |cfg| {
            cfg.checks = crate::check::Checks::from_config(&crate::check::tests::config(&crate::check::tests::recording_script(&p), "txt", None), &|_| allowed);
            tweak(cfg);
        });
        (f, probe)
    }

    #[test]
    fn checks_run_once_per_step_on_the_changed_files_and_report_in_the_last_matching_result() {
        let (f, probe) = checks_fixture(true, |_| {});
        f.fake.extend(two_edits_and_a_note(&f));
        f.say("edit things");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let log = std::fs::read_to_string(probe.join("runs.log")).unwrap();
        assert_eq!(log.lines().count(), 1, "one run for three .txt edits: {log}");
        assert!(log.contains("a.txt") && log.contains("b.txt") && !log.contains("c.md"), "{log}");
        assert_eq!(log.matches("a.txt").count(), 1, "a file edited twice is passed once");
        let out = tool_outputs(&f);
        assert!(!out[0].contains("Check fake") && !out[1].contains("Check fake"), "{out:?}");
        assert!(out[2].contains("Check fake reported problems") && out[2].contains("b.txt:1:1: error: broken"), "{}", out[2]);
        assert!(!out[3].contains("Check fake"), "the .md edit is not covered: {}", out[3]);
        let _ = std::fs::remove_dir_all(probe);
    }

    #[test]
    fn a_check_that_is_not_approved_never_runs() {
        let (f, probe) = checks_fixture(false, |_| {});
        f.fake.extend(two_edits_and_a_note(&f));
        f.say("edit things");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        assert!(!probe.join("runs.log").exists());
        assert!(tool_outputs(&f).iter().all(|o| !o.contains("Check fake")));
        let _ = std::fs::remove_dir_all(probe);
    }

    #[test]
    fn checks_false_for_an_agent_turns_them_off() {
        let (f, probe) = checks_fixture(true, |cfg| {
            for a in &mut cfg.agents {
                a.checks = false;
            }
        });
        f.fake.extend(two_edits_and_a_note(&f));
        f.say("edit things");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        assert!(!probe.join("runs.log").exists());
        let _ = std::fs::remove_dir_all(probe);
    }

    #[test]
    fn a_check_timeout_is_a_note_in_the_tool_result_not_an_error() {
        let f = fixture_cfg(vec![], |cfg| cfg.checks = crate::check::Checks::from_config(&crate::check::tests::config("sleep 5", "txt", Some(200)), &|_| true));
        f.fake.extend(two_edits_and_a_note(&f));
        f.say("edit things");
        let t = std::time::Instant::now();
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        assert!(t.elapsed() < Duration::from_secs(4));
        let out = tool_outputs(&f);
        assert!(out[2].contains("Check fake did not complete: timed out"), "{}", out[2]);
        let m = f.msgs();
        assert!(m[1].data["parts"].as_array().unwrap().iter().all(|p| p["state"]["status"] == "completed"));
    }

    /// A failure is followed by "passed" once the model fixed it, and then silence.
    #[test]
    fn a_pass_after_a_failure_is_reported_once() {
        let f = fixture_cfg(vec![], |cfg| cfg.checks = crate::check::Checks::from_config(&crate::check::tests::config("if grep -q BAD a.txt; then echo 'a.txt:1:1: error: bad'; exit 1; fi", "txt", None), &|_| true));
        f.file("a.txt", "BAD");
        let ed = |id: &str, old: &str, new: &str| Scripted::Events(vec![Event::ToolCall { id: id.into(), name: "edit".into(), input: json!({"path": "a.txt", "oldString": old, "newString": new}) }, Event::Finish { reason: FinishReason::ToolUse, usage: Usage::default() }]);
        f.fake.extend(vec![ed("w1", "BAD", "BAD2"), ed("w2", "BAD2", "fine"), ed("w3", "fine", "fine too"), text("done")]);
        f.say("go");
        let _ = f.run(&TestUi::default());
        let outs: Vec<String> = f.msgs().iter().filter(|m| m.kind == "assistant").filter_map(|m| m.data["parts"][0]["state"]["output"].as_str().map(String::from)).collect();
        assert!(outs[0].contains("reported problems"), "{outs:?}");
        assert!(outs[1].contains("Check fake passed."), "{outs:?}");
        assert!(!outs[2].contains("Check fake"), "{outs:?}");
    }

    /// A model that uses Claude Code's argument names gets the same result, and the stored call, which is what later
    /// requests replay, carries our names.
    #[test]
    fn claude_code_argument_names_work_for_read_write_and_edit() {
        let f = fixture(vec![
            tool_call("r", "read", json!({"file_path":"a.txt"})),
            tool_call("e", "edit", json!({"file_path":"a.txt","old_string":"one","new_string":"1"})),
            tool_call("m", "edit", json!({"file_path":"a.txt","edits":[{"old_string":"two","new_string":"2"}]})),
            tool_call("w", "write", json!({"file_path":"b.txt","content":"made"})),
            text("done"),
        ]);
        f.file("a.txt", "one two\n");
        f.say("go");
        let ui = TestUi::default();
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        assert_eq!(std::fs::read_to_string(f.dir.join("work/a.txt")).unwrap(), "1 2\n");
        assert_eq!(std::fs::read_to_string(f.dir.join("work/b.txt")).unwrap(), "made");
        let reqs = f.fake.requests.lock().unwrap();
        let errors: Vec<Block> = reqs.iter().flat_map(last_user_blocks).filter(|b| matches!(b, Block::ToolResult { is_error: true, .. })).collect();
        assert!(errors.is_empty(), "{errors:?}");
        // What was stored (and is replayed to the model on later steps) uses our names.
        let stored: Vec<Value> = f.msgs().into_iter().flat_map(|m| m.data["parts"].as_array().cloned().unwrap_or_default()).filter(|p| p["type"] == "tool").collect();
        let input = |id: &str| stored.iter().find(|p| p["id"] == id).unwrap()["input"].clone();
        assert_eq!(input("r"), json!({"path":"a.txt"}));
        assert_eq!(input("e"), json!({"path":"a.txt","oldString":"one","newString":"1"}));
        assert_eq!(input("m"), json!({"path":"a.txt","edits":[{"oldString":"two","newString":"2"}]}));
        assert_eq!(input("w"), json!({"path":"b.txt","content":"made"}));
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
    fn cut_off_tool_input_is_a_tool_error_and_earlier_calls_still_run() {
        let f = fixture(vec![
            Scripted::Events(vec![
                Event::ToolCall { id: "a".into(), name: "read".into(), input: json!({"path":"x"}) },
                Event::BadToolCall { id: "b".into(), name: "write".into(), raw: "{\"path\":\"y\",\"content\":\"abc".into(), error: "EOF while parsing a string".into() },
                Event::Finish { reason: FinishReason::MaxTokens, usage: Usage::default() },
            ]),
            text("smaller parts then"),
        ]);
        f.file("x", "one");
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert_eq!(reqs.len(), 2, "the loop continued after the bad call");
        let results = last_user_blocks(&reqs[1]);
        assert_eq!(results.len(), 2);
        assert!(matches!(&results[0], Block::ToolResult { id, is_error: false, .. } if id == "a"), "{:?}", results[0]);
        assert!(matches!(&results[1], Block::ToolResult { id, is_error: true, content } if id == "b" && content.contains("max_tokens") && content.contains("smaller parts")), "{:?}", results[1]);
        assert!(!f.dir.join("work").join("y").exists());
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
    fn an_always_answer_covers_the_next_call_of_the_same_step() {
        let fin = Event::Finish { reason: FinishReason::ToolUse, usage: Usage { input: 10, output: 5, ..Default::default() } };
        let ev = |id: &str| [Event::ToolInputStart { id: id.into(), name: "read".into() }, Event::ToolCall { id: id.into(), name: "read".into(), input: json!({"path":".env"}) }];
        let [a, b] = [ev("t1"), ev("t2")];
        let f = fixture(vec![Scripted::Events(a.into_iter().chain(b).chain([fin]).collect()), text("done")]);
        f.file(".env", "SECRET=1");
        f.say("go");
        // One reply only: a second prompt would panic in the test Ui.
        let ui = TestUi::replying(vec![Reply::Always]);
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        assert_eq!(ui.asked.lock().unwrap().len(), 1, "the second call in the same step is covered by the saved rule");
        let reqs = f.fake.requests.lock().unwrap();
        let blocks = last_user_blocks(&reqs[1]);
        assert!(blocks.iter().all(|b| matches!(b, Block::ToolResult { is_error: false, .. })), "{blocks:?}");
    }

    #[test]
    fn a_heredoc_body_needs_no_permission_of_its_own() {
        // Issue #84: each body line was its own resource, so a saved `cat *` still asked, and "always" saved junk
        // rules such as `tell *` for the body lines.
        let cmd = "cat <<'EOF'\ntell application \"Chess\"\n  don't click here\nend tell\nEOF";
        let script = vec![tool_call("t1", "shell", json!({"command": cmd})), tool_call("t2", "shell", json!({"command": "printf x | cat <<'EOF'\nset g to 1\nEOF"})), text("done")];
        let f = fixture_cfg(script, |c| c.agents = crate::agent::builtin_for(PermissionMode::Manual));
        f.engine.store().save_permission(&f.session.project_id, "shell", "cat *").unwrap();
        f.say("go");
        // One prompt, for `printf`; nothing for the body lines.
        let ui = TestUi::replying(vec![Reply::Always]);
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        assert_eq!(ui.asked.lock().unwrap().as_slice(), ["shell printf x,cat <<'EOF'\nset g to 1\nEOF"]);
        let reqs = f.fake.requests.lock().unwrap();
        assert!(matches!(&last_user_blocks(&reqs[1])[0], Block::ToolResult { content, is_error: false, .. } if content.contains("don't click here")), "{:?}", last_user_blocks(&reqs[1]));
        let saved: Vec<String> = f.engine.store().saved_permissions(&f.session.project_id).unwrap().into_iter().map(|r| r.resource).collect();
        assert_eq!(saved, ["cat *", "printf *"], "no rules for body lines");
    }

    #[test]
    fn the_next_action_is_suggested_from_the_last_turn_by_the_small_model() {
        let f = fixture(vec![text("I added the migration. Next I would run it against the dev database.")]);
        f.engine.store().set_todos(&f.session.id, &[Todo { content: "Write the migration".into(), status: "completed".into() }, Todo { content: "Update the README".into(), status: "pending".into() }]).unwrap();
        f.say("add a users migration");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let before = f.engine.store().usage_totals(&f.session.id).unwrap();
        f.fake.script_suggestions(vec![text("\"Run the migration on the dev database\"")]);
        let cancel = AtomicBool::new(false);
        assert_eq!(f.engine.suggest_next(&f.session.id, &cancel).as_deref(), Some("Run the migration on the dev database"));
        let reqs = f.fake.suggestion_requests.lock().unwrap();
        let Block::Text(sent) = &reqs[0].messages[0].content[0] else { panic!("text expected") };
        assert!(sent.contains("add a users migration") && sent.contains("run it against the dev database"), "{sent}");
        assert!(sent.contains("- Update the README") && !sent.contains("Write the migration"), "only open to-dos: {sent}");
        let small = f.engine.small_model_ref(&f.session);
        assert_eq!(reqs[0].model, config::split_model(&small).1, "the small model");
        drop(reqs);
        assert!(f.engine.store().usage_totals(&f.session.id).unwrap() != before, "the call's usage is added to the session");
        assert_eq!(f.fake.calls(), 1, "the main conversation saw no extra request");

        // No clear next step, a failure, or a cancel: no suggestion.
        f.fake.script_suggestions(vec![text("NONE")]);
        assert_eq!(f.engine.suggest_next(&f.session.id, &cancel), None);
        f.fake.script_suggestions(vec![Scripted::Fail(vec![], ProviderError::Auth("no".into()))]);
        assert_eq!(f.engine.suggest_next(&f.session.id, &cancel), None);
        f.fake.script_suggestions(vec![text("Run the tests")]);
        assert_eq!(f.engine.suggest_next(&f.session.id, &AtomicBool::new(true)), None);
        // A session with no reply yet has nothing to suggest from, and asks no model.
        let fresh = f.engine.store().create_session(f.dir.join("work").to_str().unwrap(), "build").unwrap();
        let asked = f.fake.suggestion_requests.lock().unwrap().len();
        assert_eq!(f.engine.suggest_next(&fresh.id, &cancel), None);
        assert_eq!(f.fake.suggestion_requests.lock().unwrap().len(), asked);
    }

    #[test]
    fn a_long_streamed_answer_is_stored_exactly_and_parts_survive_around_tool_calls() {
        let deltas: Vec<String> = (0..5000).map(|i| format!("w{i} ")).collect();
        let expected: String = deltas.concat();
        let mut events: Vec<Event> = deltas.iter().map(|d| Event::TextDelta(d.clone())).collect();
        events.push(Event::ToolInputStart { id: "t1".into(), name: "read".into() });
        events.push(Event::ToolCall { id: "t1".into(), name: "read".into(), input: json!({"path":"a.txt"}) });
        // Text after a tool call starts a new part.
        events.extend(["after ".to_string(), "call".to_string()].map(Event::TextDelta));
        events.push(Event::Finish { reason: FinishReason::ToolUse, usage: Usage { input: 10, output: 5, ..Default::default() } });
        let f = fixture(vec![Scripted::Events(events), text("done")]);
        f.file("a.txt", "hi\n");
        f.say("go");
        let ui = TestUi::default();
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        let m = f.msgs();
        let parts = m[1].data["parts"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"].as_str().unwrap(), expected);
        assert_eq!(parts[1]["type"], "tool");
        assert_eq!(parts[2]["text"], "after call");
        let shown: String = ui.log().iter().filter_map(|l| l.strip_prefix("text:")).take(5000).collect();
        assert_eq!(shown, expected, "the Ui still sees every delta");
    }

    fn notices_containing(ui: &TestUi, needle: &str) -> usize {
        ui.log().iter().filter(|l| l.starts_with("notice:") && l.contains(needle)).count()
    }

    #[test]
    fn unreadable_saved_permissions_show_one_notice_instead_of_silently_asking() {
        let f = fixture(vec![tool_call("t1", "read", json!({"path":".env"})), tool_call("t2", "read", json!({"path":".env"})), text("done")]);
        f.file(".env", "SECRET=1");
        f.say("go");
        f.engine.store().exec_for_test("alter table permission_saved rename to permission_saved_away");
        let ui = TestUi::replying(vec![Reply::Once, Reply::Once]);
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        assert_eq!(ui.asked.lock().unwrap().len(), 2, "with nothing readable, every request asks");
        assert_eq!(notices_containing(&ui, "saved permissions could not be read"), 1, "{:?}", ui.log());
    }

    #[test]
    fn an_always_answer_that_cannot_be_stored_shows_a_notice() {
        let f = fixture(vec![tool_call("t1", "read", json!({"path":".env"})), text("done")]);
        f.file(".env", "SECRET=1");
        f.say("go");
        f.engine.store().exec_for_test("create trigger no_saving before insert on permission_saved begin select raise(abort, 'disk full'); end");
        let ui = TestUi::replying(vec![Reply::Always]);
        assert_eq!(f.run(&ui), RunOutcome::Idle, "the call itself still runs");
        assert_eq!(notices_containing(&ui, "saved permissions could not be stored"), 1, "{:?}", ui.log());
        assert!(ui.log().iter().any(|l| l.contains("disk full")), "the cause is shown: {:?}", ui.log());
    }

    #[test]
    fn a_failing_usage_write_shows_exactly_one_notice_across_steps() {
        let f = fixture(vec![tool_call("t1", "read", json!({"path":"a.txt"})), tool_call("t2", "read", json!({"path":"a.txt"})), text("done")]);
        f.file("a.txt", "hi\n");
        f.say("go");
        f.engine.store().exec_for_test("create trigger no_usage before update of cost on session begin select raise(abort, 'disk full'); end");
        let ui = TestUi::default();
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        assert_eq!(ui.log().iter().filter(|l| *l == "usage").count(), 3, "three steps ran");
        assert_eq!(notices_containing(&ui, "token usage and cost could not be recorded"), 1, "{:?}", ui.log());
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
            st.orphan_claim(&f.session.id);
            assert_eq!(st.recover_claims(3).unwrap(), std::slice::from_ref(&f.session.id));
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

    /// Auto mode with the careful preset: the one place a default (soft) ask exists for the classifier to relax.
    fn auto_careful_with(settings: Option<classifier::Settings>, more: Vec<Rule>) -> impl FnOnce(&mut EngineConfig) {
        move |c| {
            let mut auto = agent::builtin_for(PermissionMode::Auto);
            for a in &mut auto {
                a.permissions.extend(permission::preset_rules("careful").unwrap());
                a.permissions.extend(more.iter().cloned());
            }
            c.permission_mode = PermissionMode::Auto;
            c.agents_by_mode = BTreeMap::from([(PermissionMode::Manual, agent::builtin_for(PermissionMode::Manual)), (PermissionMode::Auto, auto)]);
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
        ui.log().into_iter().filter(|l| l.starts_with("notice:") || l.starts_with("status:") || l.starts_with("auto:")).collect()
    }

    #[test]
    fn manual_mode_is_fully_manual_and_never_calls_the_classifier() {
        let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "echo hi"})), text("done")], auto_careful_with(Some(Default::default()), vec![]));
        f.engine.store().set_permission_mode(&f.session.id, Some("manual")).unwrap();
        f.fake.script_verdicts(vec![verdict("allow", "read-only")]);
        f.say("go");
        let ui = TestUi::replying(vec![Reply::Once]); // the one prompt is answered; a second would panic
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        assert!(f.fake.classifier_requests.lock().unwrap().is_empty(), "manual mode asks the user, nothing is classified");
        assert!(ui.replies.lock().unwrap().is_empty(), "the prompt was shown");
    }

    #[test]
    fn the_classifier_allows_a_routine_command_once_and_remembers_it() {
        let f = fixture_cfg(shell_twice("echo hi"), auto_careful_with(Some(Default::default()), vec![]));
        f.fake.script_verdicts(vec![verdict("allow", "read-only")]);
        f.say("go");
        let ui = TestUi::default(); // a prompt would panic
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        assert_eq!(f.fake.classifier_requests.lock().unwrap().len(), 1, "the second identical command is not classified again");
        let n = notices(&ui);
        assert_eq!(n[0], "auto:read-only:echo hi:because");
        assert_eq!(n[1], "auto:allowed earlier:echo hi:");
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
            auto_careful_with(Some(Default::default()), vec![])(c);
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
            auto_careful_with(Some(classifier::Settings { model: Some("anthropic/claude-unpriced".into()), ..Default::default() }), vec![])(c);
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
            let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "make"})), text("done")], auto_careful_with(Some(Default::default()), vec![]));
            f.fake.script_verdicts(reply.into_iter().collect());
            f.say("go");
            let ui = TestUi::replying(vec![Reply::Once]);
            assert_eq!(f.run(&ui), RunOutcome::Idle, "{why}");
            assert_eq!(ui.asked.lock().unwrap().len(), 1, "{why}");
            assert!(notices(&ui).contains(&format!("status:classifier: asking you because {why}")), "{why}: {:?}", notices(&ui));
        }
        // A refusal is remembered nowhere: the same command is classified (and asked) again.
        let f = fixture_cfg(shell_twice("make"), auto_careful_with(Some(Default::default()), vec![]));
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
            let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": cmd})), text("done")], auto_careful_with(Some(Default::default()), rules));
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
        let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "make"})), text("done")], auto_careful_with(Some(Default::default()), vec![Rule::new("shell", "make", Effect::Deny)]));
        f.fake.script_verdicts(vec![verdict("allow", "read-only")]);
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        assert!(matches!(&last_user_blocks(&f.fake.requests.lock().unwrap()[1])[0], Block::ToolResult { is_error: true, content, .. } if content.starts_with("Permission denied: shell")));
        assert!(f.fake.classifier_requests.lock().unwrap().is_empty());

        let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "echo hi"})), text("done")], auto_careful_with(None, vec![]));
        f.say("go");
        let ui = TestUi::replying(vec![Reply::Once]);
        f.run(&ui);
        assert_eq!(ui.asked.lock().unwrap().len(), 1);
        assert!(f.fake.classifier_requests.lock().unwrap().is_empty());
    }

    #[test]
    fn a_slow_classifier_times_out_into_a_prompt() {
        let settings = classifier::Settings { timeout: Duration::from_millis(150), ..Default::default() };
        let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "echo hi"})), text("done")], auto_careful_with(Some(settings), vec![]));
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

    /// The classifier lines logged for one session, oldest first (the `session=` field is unique per test).
    fn classifier_log(sid: &str, level: &str) -> Vec<String> {
        crate::log::capture::global_lines(&format!("session={sid}")).into_iter().filter(|l| l.starts_with(&format!("{level} classifier:"))).collect()
    }

    #[test]
    fn every_classifier_decision_is_logged_and_listed_by_permissions() {
        crate::log::capture::ensure_global("debug");
        let f = fixture_cfg(shell_twice("echo hi"), auto_careful_with(Some(Default::default()), vec![]));
        f.fake.script_verdicts(vec![verdict("allow", "read-only")]);
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let sid = &f.session.id;
        let lines = classifier_log(sid, "INFO");
        assert_eq!(lines.len(), 2, "one line per decision, the repeat included: {lines:?}");
        for part in ["decision=allow", "model_verdict=allow", "risk=read-only", "model=anthropic/claude-haiku-5-5", " ms=", " input=10 output=5 ", "cost=", "reason=because", "command=echo hi"] {
            assert!(lines[0].contains(part), "{part} missing from {}", lines[0]);
        }
        // Issue #71: `tokens_in=` matched the secret-assignment rule, so the counts were logged as `[redacted]`.
        assert!(!lines[0].contains("[redacted]"), "nothing in a classifier line about `echo hi` is secret: {}", lines[0]);
        assert!(lines[1].contains("decision=allow cached=true") && lines[1].contains("command=echo hi"), "{}", lines[1]);
        assert!(!classifier_log(sid, "DEBUG").is_empty(), "the raw reply is kept at debug: {:?}", crate::log::capture::global_lines(&format!("session={sid}")));

        // What `/permissions` shows: the mode, the model, the command (once, not per repeat) and the saved rules.
        let report = f.engine.permissions_report(sid).unwrap();
        assert!(report[0].contains("permission mode: auto") && report[0].contains("on (anthropic/claude-haiku-5-5)"), "{report:?}");
        assert_eq!(report[1], "allowed automatically in this session: 1");
        assert!(report[2].contains("read-only") && report[2].contains("echo hi") && report[2].contains("(because)"), "{report:?}");
        assert_eq!(report[3], "saved \"always allow\" rules for this project: 0");
        f.engine.store().save_permission(&f.session.project_id, "shell", "cargo test *").unwrap();
        assert!(f.engine.permissions_report(sid).unwrap().iter().any(|l| l.contains("shell") && l.contains("cargo test *")));

        // The other states are described honestly.
        f.engine.store().set_permission_mode(sid, Some("manual")).unwrap();
        assert!(f.engine.permissions_report(sid).unwrap()[0].contains("idle (it only works in auto mode)"));
        let off = fixture_cfg(vec![text("hi")], auto_careful_with(None, vec![]));
        assert!(off.engine.permissions_report(&off.session.id).unwrap()[0].contains("off (permission_classifier.enabled is false)"));
    }

    #[test]
    fn declines_timeouts_and_skips_are_logged_with_their_cause() {
        crate::log::capture::ensure_global("debug");
        // The model advises against it: logged with the model's own words, and the user is asked.
        let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "make"})), text("done")], auto_careful_with(Some(Default::default()), vec![]));
        f.fake.script_verdicts(vec![verdict("deny", "destructive")]);
        f.say("go");
        assert_eq!(f.run(&TestUi::replying(vec![Reply::Once])), RunOutcome::Idle);
        let lines = classifier_log(&f.session.id, "INFO");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("decision=ask model_verdict=deny risk=destructive") && lines[0].contains("command=make"), "{}", lines[0]);
        assert_eq!(f.engine.store().auto_allows(&f.session.id).unwrap().len(), 0, "only allows are listed");

        // No answer in time: the cause is in the log.
        let settings = classifier::Settings { timeout: Duration::from_millis(150), ..Default::default() };
        let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "echo hi"})), text("done")], auto_careful_with(Some(settings), vec![]));
        f.fake.delay_classifier(Duration::from_secs(30));
        f.fake.script_verdicts(vec![verdict("allow", "read-only")]);
        f.say("go");
        assert_eq!(f.run(&TestUi::replying(vec![Reply::Once])), RunOutcome::Idle);
        let lines = classifier_log(&f.session.id, "INFO");
        assert!(lines.len() == 1 && lines[0].contains("decision=ask") && lines[0].contains("no answer within 150 ms"), "{lines:?}");

        // Commands that are never sent to the model leave a debug line saying why, and no verdict.
        let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "sudo ls"})), text("done")], auto_careful_with(Some(Default::default()), vec![]));
        f.say("go");
        // Reject the prompt: approving it would really run `sudo`, which waits for a password on the terminal.
        assert_eq!(f.run(&TestUi::replying(vec![Reply::Reject(None)])), RunOutcome::Declined);
        assert!(classifier_log(&f.session.id, "INFO").is_empty());
        let skipped = classifier_log(&f.session.id, "DEBUG");
        assert!(skipped.len() == 1 && skipped[0].contains("skipped (opaque or never-classify list)") && skipped[0].contains("sudo ls"), "{skipped:?}");
    }

    #[test]
    fn an_interrupt_while_classifying_is_not_reported_as_a_timeout_or_logged_as_a_decision() {
        crate::log::capture::ensure_global("debug");
        let settings = classifier::Settings { timeout: Duration::from_secs(20), ..Default::default() };
        let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "echo hi"})), text("done")], auto_careful_with(Some(settings), vec![]));
        f.fake.delay_classifier(Duration::from_secs(30));
        f.fake.script_verdicts(vec![verdict("allow", "read-only")]);
        f.say("go");
        let cancel = Arc::new(AtomicBool::new(false));
        let ui = TestUi::replying(vec![Reply::Once]);
        let (c2, started) = (cancel.clone(), std::time::Instant::now());
        let esc = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            c2.store(true, Ordering::Relaxed);
        });
        let _ = f.engine.run(&f.session.id, &ui, &cancel, false);
        esc.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(10), "the interrupt ends the wait at once, not at the 20 s limit");
        assert!(!notices(&ui).iter().any(|n| n.contains("did not answer in time") || n.contains("classifier: asking you")), "{:?}", notices(&ui));
        assert!(classifier_log(&f.session.id, "INFO").is_empty(), "no decision was made, so none is logged: {:?}", classifier_log(&f.session.id, "INFO"));
        assert!(classifier_log(&f.session.id, "DEBUG").iter().any(|l| l.contains("interrupted") && l.contains("no decision made")));
    }

    #[test]
    fn permissions_for_a_session_that_does_not_exist_is_a_message_not_a_panic() {
        let f = fixture(vec![]);
        let report = f.engine.permissions_report("ses_missing").unwrap();
        assert_eq!(report.len(), 1);
        assert!(report[0].contains("ses_missing") && report[0].contains("not found"), "{report:?}");
    }

    #[test]
    fn the_classifier_model_comes_from_its_setting_then_the_small_model() {
        let settings = classifier::Settings { model: Some("anthropic/claude-judge".into()), ..Default::default() };
        let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "echo hi"})), text("done")], |c| {
            auto_careful_with(Some(settings), vec![])(c);
            c.small_model = Some("anthropic/claude-small".into());
        });
        f.fake.script_verdicts(vec![verdict("allow", "read-only")]);
        f.say("go");
        f.run(&TestUi::default());
        assert_eq!(f.fake.classifier_requests.lock().unwrap()[0].model, "claude-judge");

        let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "echo hi"})), text("done")], |c| {
            auto_careful_with(Some(Default::default()), vec![])(c);
            c.small_model = Some("anthropic/claude-small".into());
        });
        f.fake.script_verdicts(vec![verdict("allow", "read-only")]);
        f.say("go");
        f.run(&TestUi::default());
        assert_eq!(f.fake.classifier_requests.lock().unwrap()[0].model, "claude-small");
    }

    /// Thinking tokens count against `max_tokens`. Haiku 5.5 thinks whatever the request says: with a budget sized for the
    /// answer alone, the classifier's reply came back empty or cut off in the middle (found by running the evaluation).
    #[test]
    fn short_answers_leave_room_for_models_that_cannot_stop_thinking() {
        let f = fixture(vec![]);
        let shape = |m: &str, n: u32| f.engine.short_answer_shape("anthropic", m, n);
        // Cannot be switched off: the lowest effort, and room to think before the answer.
        assert_eq!(shape("claude-haiku-5-5", 200), (Some("low".into()), 200 + THINKING_HEADROOM));
        assert_eq!(shape("claude-opus-5-5", 60), (Some("low".into()), 60 + THINKING_HEADROOM));
        // Can be switched off (or only runs between tool calls): switched off, and the budget stays small.
        assert_eq!(shape("claude-opus-4-7", 200), (Some("none".into()), 200));
        assert_eq!(shape("claude-sonnet-5-5", 200), (Some("none".into()), 200));
        // A model the engine knows nothing about is asked as before.
        assert_eq!(shape("claude-judge", 200), (None, 200));
        assert_eq!(f.engine.short_answer_shape("nowhere", "any", 60), (None, 60));
    }

    #[test]
    fn the_classifier_and_the_title_use_that_shape() {
        let settings = classifier::Settings { model: Some("anthropic/claude-haiku-5-5".into()), ..Default::default() };
        let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "echo hi"})), text("done")], |c| {
            auto_careful_with(Some(settings), vec![])(c);
            c.small_model = Some("anthropic/claude-haiku-5-5".into());
        });
        f.fake.script_verdicts(vec![verdict("allow", "read-only")]);
        f.fake.script_titles(vec![text("A title")]);
        f.say("go");
        f.run(&TestUi::default());
        let c = f.fake.classifier_requests.lock().unwrap()[0].clone();
        assert_eq!((c.effort.as_deref(), c.max_tokens), (Some("low"), 200 + THINKING_HEADROOM));
        let t = titles(&f)[0].clone();
        assert_eq!((t.effort.as_deref(), t.max_tokens), (Some("low"), 60 + THINKING_HEADROOM));
        // The conversation's own requests are not touched.
        assert_eq!(f.fake.requests.lock().unwrap()[0].effort, None);
    }

    #[test]
    fn a_reply_cut_off_at_the_token_limit_says_so() {
        for partial in ["{\"verdict\": \"al", ""] {
            let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "echo hi"})), text("done")], auto_careful_with(Some(Default::default()), vec![]));
            let mut events = if partial.is_empty() { vec![] } else { vec![Event::TextDelta(partial.into())] };
            events.push(Event::Finish { reason: FinishReason::MaxTokens, usage: Usage::default() });
            f.fake.script_verdicts(vec![Scripted::Events(events)]);
            f.say("go");
            let ui = TestUi::replying(vec![Reply::Once]);
            assert_eq!(f.run(&ui), RunOutcome::Idle);
            assert!(notices(&ui).iter().any(|n| n.contains("the classifier's reply was cut off at its token limit")), "{partial:?}: {:?}", notices(&ui));
            assert_eq!(ui.asked.lock().unwrap().len(), 1, "the user is asked");
        }
        // A garbled reply that was not cut off keeps its own message.
        let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "echo hi"})), text("done")], auto_careful_with(Some(Default::default()), vec![]));
        f.fake.script_verdicts(vec![text("I think this is fine")]);
        f.say("go");
        let ui = TestUi::replying(vec![Reply::Once]);
        f.run(&ui);
        assert!(notices(&ui).iter().any(|n| n.contains("no usable answer")), "{:?}", notices(&ui));
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
            let f = fixture_cfg(vec![tool_call("t1", "shell", json!({"command": "echo hi"})), text("done")], auto_careful_with(Some(settings), vec![]));
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

    #[test]
    fn the_stored_todo_list_is_shown_to_the_model_while_it_has_open_items() {
        let todo = |c: &str, s: &str| crate::tools::Todo { content: c.into(), status: s.into() };
        let steps = || vec![tool_call("t1", "read", json!({"path":"a.txt"})), tool_call("t2", "glob", json!({"pattern":"*.txt"})), text("done")];
        for agent in ["build", "plan"] {
            // Open items: every request of the turn carries the list, identical each time, and it is not a stored message.
            let f = fixture(steps());
            f.engine.store().set_agent(&f.session.id, agent).unwrap();
            f.engine.store().set_todos(&f.session.id, &[todo("write tests", "in_progress"), todo("ship", "pending"), todo("plan", "completed")]).unwrap();
            f.file("a.txt", "x\n");
            f.say("go on");
            assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
            let reqs = f.fake.requests.lock().unwrap();
            assert_eq!(reqs.len(), 3);
            let tail = reqs[0].tail.clone().unwrap_or_else(|| panic!("{agent}: no reminder"));
            assert!(tail.contains("- [in_progress] write tests\n- [pending] ship\n- [completed] plan\n"), "{tail}");
            for w in reqs.windows(2) {
                assert_eq!(w[1].tail.as_deref(), Some(tail.as_str()), "{agent}: an unchanged list gives the same tail");
                assert_eq!(w[0].system, w[1].system, "{agent}: system prompt");
                assert_eq!(w[0].tools, w[1].tools, "{agent}: tools");
                assert_eq!(&w[1].messages[..w[0].messages.len()], &w[0].messages[..], "{agent}: earlier messages");
            }
            assert!(!reqs.iter().any(|r| all_text(r).contains("todo list")), "{agent}: the reminder is not in the messages");
            assert!(!f.msgs().iter().any(|m| m.data.to_string().contains("system-reminder")), "{agent}: the reminder is never stored");
        }
        // An empty list, and one with nothing left to do, add nothing.
        let f = fixture(steps());
        f.file("a.txt", "x\n");
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        assert!(f.fake.requests.lock().unwrap().iter().all(|r| r.tail.is_none()));
        let f = fixture(steps());
        f.engine.store().set_todos(&f.session.id, &[todo("a", "completed"), todo("b", "cancelled")]).unwrap();
        f.file("a.txt", "x\n");
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        assert!(f.fake.requests.lock().unwrap().iter().all(|r| r.tail.is_none()));
    }

    #[test]
    fn a_todowrite_in_the_turn_changes_only_the_tail() {
        let f = fixture(vec![
            tool_call("t1", "read", json!({"path":"a.txt"})),
            tool_call("t2", "todowrite", json!({"todos":[{"content":"a","status":"in_progress"}]})),
            tool_call("t3", "glob", json!({"pattern":"*.txt"})),
            tool_call("t4", "todowrite", json!({"todos":[{"content":"a","status":"completed"}]})),
            text("done"),
        ]);
        f.file("a.txt", "x\n");
        f.say("go");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        let tails: Vec<bool> = reqs.iter().map(|r| r.tail.as_deref().is_some_and(|t| t.contains("[in_progress] a"))).collect();
        assert_eq!(tails, [false, false, true, true, false], "no list, then the list, then finished");
        for w in reqs.windows(2) {
            assert_eq!(&w[1].messages[..w[0].messages.len()], &w[0].messages[..], "the cached prefix never changes");
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
        assert_eq!(f.engine.prune_snapshots(std::slice::from_ref(&pid), Duration::from_secs(3600)), (0, 0));
        // With a zero window every session counts as stale: old snapshots go (the files on disk never do). Blobs
        // written within the grace period are kept for a capture in progress, so age them past it first.
        let blobs_dir = f.dir.join("app/data/snapshots").join(&pid).join("blobs");
        let past = std::time::SystemTime::now() - Duration::from_secs(2 * 3600);
        for shard in std::fs::read_dir(&blobs_dir).unwrap().filter_map(|e| e.ok()) {
            for b in std::fs::read_dir(shard.path()).unwrap().filter_map(|e| e.ok()) {
                std::fs::File::options().write(true).open(b.path()).unwrap().set_modified(past).unwrap();
            }
        }
        // The session was last updated at the end of the run, possibly within this millisecond, which a zero window
        // still counts as recent. Let the clock move on so the session really is outside the window.
        std::thread::sleep(Duration::from_millis(5));
        let (manifests, blobs) = f.engine.prune_snapshots(std::slice::from_ref(&pid), Duration::ZERO);
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
        let f = fixture(vec![bg_shell("t1", "sleep 2; echo done-bg; exit 3"), text("it is running"), text("noted the result")]);
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
        assert!(t.contains("Background command finished (shell ID: shl_") && t.contains("exited (code 3)") && t.contains("Command: sleep 2; echo done-bg; exit 3") && t.contains("done-bg"), "{t}");
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

    // ---- spending guardrails (`limits`) ----

    /// One scripted step of a priced model costs this much: 10 input and 5 output tokens at $0.10 / $0.50 per million.
    const STEP_COST: f64 = (10.0 * 0.1 + 5.0 * 0.5) / 1e6;

    fn read_loop(n: usize) -> Vec<Scripted> {
        (0..n).map(|i| tool_call(&format!("t{i}"), "read", json!({"path": "a.txt"}))).collect()
    }

    /// A fixture on a priced model with these limits.
    fn limited(script: Vec<Scripted>, limits: config::Limits) -> Fixture {
        let f = fixture_cfg(script, move |c| {
            c.model = PRICED.into();
            c.limits = limits;
        });
        f.file("a.txt", "hi\n");
        f
    }

    fn what_of(o: RunOutcome) -> String {
        match o {
            RunOutcome::LimitReached { what } => what,
            other => panic!("expected a limit to stop the run, got {other:?}"),
        }
    }

    #[test]
    fn a_looping_tool_call_script_stops_at_max_steps() {
        let mut script = read_loop(3);
        script.push(text("summary of the work"));
        let f = limited(script, config::Limits { max_steps: Some(3), ..Default::default() });
        f.say("loop");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let reqs = f.fake.requests.lock().unwrap();
        assert_eq!(reqs.len(), 4, "three steps with tools, then the summary step");
        assert!(reqs[..3].iter().all(|r| !r.tools_disabled && !all_text(r).contains("MAXIMUM STEPS REACHED")));
        assert!(reqs[3].tools_disabled && all_text(&reqs[3]).contains("MAXIMUM STEPS REACHED"), "the existing max-steps path");
    }

    #[test]
    fn an_agents_own_steps_beat_the_default_and_a_model_that_ignores_the_summary_step_is_stopped() {
        let mut script = read_loop(2);
        script.push(text("never reached"));
        let f = fixture_cfg(script, |c| {
            c.limits = config::Limits { max_steps: Some(5), ..Default::default() };
            c.agents.iter_mut().for_each(|a| a.steps = Some(1));
        });
        f.file("a.txt", "hi\n");
        f.say("loop");
        // Step 0 uses a tool; step 1 has tools disabled but the model calls one anyway.
        let what = what_of(f.run(&TestUi::default()));
        assert!(what.contains("step limit (1)"), "{what}");
        assert_eq!(f.fake.calls(), 2);
    }

    #[test]
    fn a_run_without_limits_is_not_stopped() {
        let mut script = read_loop(30);
        script.push(text("done"));
        let f = limited(script, config::Limits::default());
        f.say("loop");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        assert_eq!(f.fake.calls(), 31);
    }

    #[test]
    fn max_cost_per_run_stops_the_run_and_counts_a_subagents_spend() {
        let sub = json!({"agent": "general", "description": "d", "prompt": "SUB-COST do it"});
        // Own spend after step one: one step. With the subagent's step: two. The limit sits in between.
        let limit = 1.5 * STEP_COST;
        let f = limited(vec![tool_call("t1", "subagent", sub.clone()), text("never reached")], config::Limits { max_cost_per_run: Some(limit), ..Default::default() });
        f.fake.script_route("SUB-COST", vec![text("sub done")]);
        f.say("delegate");
        let what = what_of(f.run(&TestUi::replying(vec![Reply::Reject(None)]))); // asked whether to go on: stop
        assert!(what.starts_with("This turn has cost $0.000007 (limit $0.000005)"), "{what}");
        assert_eq!(f.fake.calls(), 1, "the parent did not take another step");
        let (total, _) = f.engine.store().usage_totals(&f.session.id).unwrap();
        assert!((total - 2.0 * STEP_COST).abs() < 1e-12, "the subagent's step is in the parent's total: {total}");
        // The same work under a limit that covers both steps goes through.
        let g = limited(vec![tool_call("t1", "subagent", sub), text("all done")], config::Limits { max_cost_per_run: Some(2.5 * STEP_COST), ..Default::default() });
        g.fake.script_route("SUB-COST", vec![text("sub done")]);
        g.say("delegate");
        assert_eq!(g.run(&TestUi::default()), RunOutcome::Idle);
    }

    #[test]
    fn the_run_allowance_starts_again_with_the_next_prompt() {
        let mut script = read_loop(2);
        script.push(text("first done"));
        script.push(tool_call("u1", "read", json!({"path": "a.txt"})));
        script.push(text("second done"));
        let f = limited(script, config::Limits { max_cost_per_run: Some(3.0 * STEP_COST), ..Default::default() });
        f.say("one");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle); // three steps = exactly the limit, not over it
        f.say("two");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle, "earlier runs do not count toward this run");
    }

    #[test]
    fn an_attended_user_can_continue_continue_without_the_limit_or_stop() {
        // Over the limit after the 2nd step: "continue" gives another allowance; after the 4th the user stops it.
        let mut script = read_loop(4);
        script.push(text("never reached"));
        let f = limited(script, config::Limits { max_cost_per_run: Some(1.5 * STEP_COST), ..Default::default() });
        f.say("go");
        let ui = TestUi::replying(vec![Reply::Once, Reply::Reject(None)]);
        let what = what_of(f.run(&ui));
        assert!(what.starts_with("This turn has cost"), "{what}");
        assert_eq!(f.fake.calls(), 4);
        let asked = ui.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 2);
        assert!(asked.iter().all(|a| a.starts_with("limit This turn has cost $0.000007 (limit $0.000005)")), "{asked:?}");

        // "Without the limit" is asked once and holds for the rest of the run.
        let mut script = read_loop(5);
        script.push(text("finished"));
        let g = limited(script, config::Limits { max_cost_per_run: Some(1.5 * STEP_COST), ..Default::default() });
        g.say("go");
        let ui = TestUi::replying(vec![Reply::Always]);
        assert_eq!(g.run(&ui), RunOutcome::Idle);
        assert_eq!(ui.asked.lock().unwrap().len(), 1);
        assert_eq!(g.fake.calls(), 6);
    }

    #[test]
    fn max_cost_per_session_stops_a_later_run() {
        let mut script = vec![tool_call("a1", "read", json!({"path": "a.txt"})), text("first done")];
        script.extend([tool_call("b1", "read", json!({"path": "a.txt"})), text("never reached")]);
        let f = limited(script, config::Limits { max_cost_per_session: Some(2.5 * STEP_COST), ..Default::default() });
        f.say("one");
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle, "two steps stay under the limit");
        f.say("two");
        let what = what_of(f.run(&TestUi::replying(vec![Reply::Reject(None)])));
        assert!(what.starts_with("This session has cost $0.00001") && what.ends_with("(limit $0.000009)"), "{what}");
        assert_eq!(f.fake.calls(), 3, "the second run stopped after its first step");
    }

    #[test]
    fn a_background_subagent_stops_with_limit_reached_and_the_parent_is_told() {
        let f = limited(vec![text("the parent saw the report")], config::Limits { max_cost_per_run: Some(2.5 * STEP_COST), ..Default::default() });
        let mut script = read_loop(5);
        script.push(text("never reached"));
        f.fake.script_route("BG-LIMIT", script);
        let req = SubagentRequest { agent: "general".into(), description: "costly task".into(), prompt: "BG-LIMIT do the work".into(), model: None, session_id: None };
        let child = f.engine.begin_subagent(&f.session, &req).unwrap();
        f.engine.start_subagent_job(&f.session, &child, &req.description);
        wait_until("the subagent to stop", || f.engine.running_jobs() == 0);
        let status = f.engine.background_jobs(None)[0].status.clone();
        assert!(status.starts_with("stopped at a spending limit: This turn has cost"), "{status}");
        assert_eq!(f.fake.routed_requests.lock().unwrap().len(), 3, "it stopped after the step that passed the limit");
        // The parent gets the synthetic message with its next run.
        assert_eq!(f.engine.store().pending_inbox(&f.session.id).unwrap(), 1);
        assert_eq!(f.run(&TestUi::default()), RunOutcome::Idle);
        let t = all_text(&f.fake.requests.lock().unwrap()[0]);
        assert!(t.contains("Background subagent stopped at a spending limit (sessionID: ses_") && t.contains("task: costly task") && t.contains("This turn has cost"), "{t}");
    }

    #[test]
    fn an_unpriced_model_is_said_so_once_and_never_stopped_by_cost() {
        let mut script = read_loop(3);
        script.push(text("done"));
        let f = fixture_cfg(script, |c| c.limits = config::Limits { max_cost_per_run: Some(0.0), max_cost_per_session: Some(0.0), ..Default::default() });
        f.file("a.txt", "hi\n");
        f.say("go");
        let ui = TestUi::default();
        assert_eq!(f.run(&ui), RunOutcome::Idle);
        let said: Vec<String> = ui.log().into_iter().filter(|l| l.contains("cost limit cannot be enforced")).collect();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("test-model"));
        // Without a cost limit nothing is said.
        let g = fixture(vec![text("done")]);
        g.say("go");
        let ui = TestUi::default();
        g.run(&ui);
        assert!(!ui.log().iter().any(|l| l.contains("cost limit")));
    }
}
