//! The step loop: one LLM call per step, request building and the streaming attempt.
use super::*;

struct Prepared<'a> {
    session: Session,
    agent: Agent,
    req: Request,
    provider: &'a dyn Provider,
    provider_id: String,
    model_id: String,
    /// History after the latest compaction.
    eff: Vec<crate::store::Message>,
    /// The full stored history `eff` was cut from, so auto-compaction can reuse it instead of reloading.
    history: Vec<crate::store::Message>,
    limit: Option<u64>,
    /// The tool list the request was built from (tool calls are looked up here).
    tools: Vec<Arc<dyn Tool>>,
}

/// One line per finished tool call: name, outcome, duration and the size of what the model gets back.
fn log_tool(name: &str, outcome: &Result<ToolOutput, ToolError>, elapsed: Duration) {
    let (status, size) = match outcome {
        Ok(o) => ("ok", o.text.len()),
        Err(ToolError::Failure(m)) => ("error", m.len()),
        Err(ToolError::Declined) => ("declined", 0),
        Err(ToolError::Corrected(m)) => ("corrected", m.len()),
        Err(ToolError::Blocked(m)) => ("blocked", m.len()),
        Err(ToolError::Interrupted) => ("interrupted", 0),
    };
    crate::log_debug!("tool", "{name} {status} ms={} output_bytes={size}", elapsed.as_millis());
}

pub(super) enum StepOutcome {
    Continue,
    Done,
    /// The model stopped at the output token limit without calling a tool; `answered` says whether any answer text
    /// came before the cut (otherwise thinking used up the budget). `drain` asks it to go on (#148).
    CutOff { answered: bool },
    Stop(RunOutcome),
}

/// How many times in a row a reply cut off at the output token limit is continued before the turn ends.
const MAX_CONTINUATIONS: u32 = 2;

/// The stored message that asks the model to go on after its reply was cut off at the output token limit.
fn continuation_text(answered: bool) -> &'static str {
    if answered {
        "Your previous response was cut off at the output token limit. Continue exactly where it stopped, without repeating what you already wrote. If a lot remains, write it in smaller parts."
    } else {
        "Your previous response used up the output token limit while thinking, before any answer was written. Think less this time and answer now; if the answer is long, write it in smaller parts."
    }
}

struct AttemptResult {
    parts: Vec<Value>,
    stream: Result<(), ProviderError>,
    finish: Option<(FinishReason, Usage)>,
    dispatched: bool,
    declined: bool,
}

impl Engine {
    pub(super) fn drain(&self, sid: &str, ui: &dyn Ui, cancel: &AtomicBool, resume: bool) -> Result<RunOutcome, StoreError> {
        let (mut continuing, mut step, mut cut_offs) = (resume, 0u32, 0u32);
        let _budget = self.limits_begin(sid);
        loop {
            let delivered = self.store().deliver(sid, !continuing)?;
            if !delivered.is_empty() {
                step = 0; // new user input resets the step allowance
                cut_offs = 0;
                self.limits_new_input(sid); // and the cost allowance of the run
                ui.event(&UiEvent::Delivered);
            }
            if !continuing && delivered.is_empty() {
                return Ok(RunOutcome::Idle);
            }
            if cancel.load(Ordering::Relaxed) {
                return Ok(RunOutcome::Interrupted);
            }
            match self.run_step(sid, step, ui, cancel)? {
                StepOutcome::Continue => (continuing, cut_offs) = (true, 0),
                StepOutcome::Done => continuing = false,
                StepOutcome::CutOff { answered } => {
                    if cut_offs >= MAX_CONTINUATIONS {
                        ui.event(&UiEvent::Notice("response stopped at the output token limit"));
                        continuing = false;
                    } else {
                        // Stored (not appended at request time) so the next request extends this one and a resumed
                        // session sees the same conversation.
                        cut_offs += 1;
                        self.store().add_message(sid, "synthetic", &json!({"text": continuation_text(answered), "description": "continuation"}))?;
                        ui.event(&UiEvent::Notice("response reached the output token limit; asking the model to continue"));
                        if let Some(o) = self.check_limits(sid, ui) {
                            return Ok(o);
                        }
                        continuing = true;
                    }
                }
                StepOutcome::Stop(o) => return Ok(o),
            }
            step += 1;
        }
    }

    fn run_step(&self, sid: &str, step: u32, ui: &dyn Ui, cancel: &AtomicBool) -> Result<StepOutcome, StoreError> {
        self.run_step_inner(sid, step, ui, cancel, false)
    }

    pub(super) fn resolve_model(&self, session: &Session, agent: &Agent) -> Result<(&dyn Provider, String, String), String> {
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
            (st.session(sid)?.ok_or(StoreError::SessionGone)?, st.messages(sid)?)
        };
        self.prepare_with(sid, step, ui, session, history)
    }

    fn prepare_with(&self, sid: &str, step: u32, ui: &dyn Ui, session: Session, history: Vec<crate::store::Message>) -> Result<std::result::Result<Prepared<'_>, StepOutcome>, StoreError> {
        let agents = self.agents_of(&session);
        let agent = agent::find(agents, &session.agent).or_else(|| agents.first().cloned()).expect("at least one agent is configured");
        let all_tools = self.current_tools();
        let advertised: Vec<&dyn Tool> = all_tools.iter().map(|t| t.as_ref()).filter(|t| agent::advertised(&agent, *t)).collect();

        let eff = compaction::effective(&history);
        let mut messages = self.chat(&eff);
        let mut reminders = vec![];
        let max_steps_hit = agent::effective_steps(&agent, self.cfg.limits.max_steps).is_some_and(|n| step >= n);
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
        let loaded = instructions::load(&self.cfg.app_dir, std::path::Path::new(&session.directory), &self.cfg.instructions, &lazy);
        if step == 0 {
            // Once per turn, not on every step of it.
            for p in &loaded.rejected {
                let msg = format!("ignored instruction file {}: it resolves outside the project", p.display());
                crate::log_warn!("instructions", "{msg}");
                ui.event(&UiEvent::Notice(&msg));
            }
        }
        let ins = instructions::render(&loaded.items);
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
            &agent::EnvInfo { cwd: &session.directory, platform: crate::sysinfo::describe(), date: &agent::date_from_unix(now), instructions: &ins, skills: &skill_list },
        );
        if agent.id == "plan" {
            // In the system prompt (not appended to the last message) so every request keeps the same prefix.
            system.push('\n');
            system.push_str(agent::PLAN_REMINDER);
            system.push('\n');
        }
        let (effort, overlay) = self.request_extras(&session, &agent, &provider_id, &model_id);
        // The stored list, so the model sees it even when its own todowrite calls are far back or were compacted
        // away. In `tail`, after the cached prefix, so it can change without a cache miss.
        let tail = if advertised.iter().any(|t| t.name() == "todowrite") { agent::todo_reminder(&self.store().todos(sid)?) } else { None };
        let mut req = Request {
            model: model_id.clone(),
            system,
            messages,
            tail,
            tools: advertised.iter().map(|t| ToolDef { name: t.name().into(), description: t.description(), schema: t.schema() }).collect(),
            max_tokens: 0,
            tools_disabled: max_steps_hit,
            effort,
            overlay,
        };
        let limit = self.cfg.catalog.model(&provider_id, &model_id).map(|m| m.limit.context).filter(|l| *l > 0);
        req.max_tokens = fit_output(self.output_budget(&provider_id, &model_id), limit, compaction::context_size(&eff, compaction::estimate_request(&req)));
        Ok(Ok(Prepared { session, agent, req, provider, provider_id, model_id, eff, history, limit, tools: all_tools }))
    }

    pub(super) fn run_step_inner(&self, sid: &str, step: u32, ui: &dyn Ui, cancel: &AtomicBool, overflowed: bool) -> Result<StepOutcome, StoreError> {
        let mut p = match self.prepare(sid, step, ui)? {
            Ok(p) => p,
            Err(outcome) => return Ok(outcome),
        };

        // Auto compaction: the prompt is close to the model's limit.
        if let (true, Some(limit)) = (self.cfg.compaction.auto, p.limit) {
            let size = compaction::context_size(&p.eff, compaction::estimate_request(&p.req));
            if size > self.cfg.compaction.threshold(limit) && !self.auto_failed.lock().unwrap().contains(sid) {
                match self.compact_with(sid, "auto", p.session.clone(), &p.history, ui, cancel)? {
                    CompactResult::Compacted { .. } => {
                        let session = {
                            let st = self.store();
                            let session = st.session(sid)?.ok_or(StoreError::SessionGone)?;
                            // Include the checkpoint and user shell results written while summarizing, without
                            // parsing the old messages again.
                            let after = p.history.last().map(|m| m.id.as_str()).unwrap_or("");
                            let added = st.messages_after(sid, after)?;
                            p.history.extend(added);
                            session
                        };
                        p = match self.prepare_with(sid, step, ui, session, std::mem::take(&mut p.history))? {
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
        let tools_disabled = p.req.tools_disabled;
        let Prepared { session, agent, req, provider, provider_id, model_id, tools: step_tools, .. } = p;
        let (provider_id, model_id) = (provider_id.as_str(), model_id.as_str());
        let caps = self.media_caps(provider_id, model_id);
        crate::log_debug!("engine", "step session={sid} step={step} agent={} model={provider_id}/{model_id} messages={} tools={}", agent.id, req.messages.len(), req.tools.len());

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
            notified: &self.notified,
            // Manual mode is fully manual: the classifier only ever works in auto mode.
            classify: self.cfg.classifier.as_ref().filter(|_| self.permission_mode_of(&session) == PermissionMode::Auto).filter(|s| s.background || !ui.unattended()).map(|settings| classify::ClassifyCtx { engine: self, session: &session, settings, cancel }),
            cache: Mutex::new(None),
        };

        let mut retries = 0u32;
        let result = loop {
            let r = self.attempt(provider, &session, &req, &permitter, ui, cancel, &msg_id, caps, &step_tools, agent.checks);
            match &r.stream {
                Err(e) if !r.dispatched && !cancel.load(Ordering::Relaxed) => {
                    // Safe to retry: no tool has run, and the partial output is discarded.
                    retries += 1;
                    if let Some(wait) = retry::delay(retries, e, (self.cfg.jitter)()) {
                        crate::log_warn!("engine", "retry session={sid} step={step} attempt={retries} wait_ms={}: {}", wait.as_millis(), e.message());
                        ui.event(&UiEvent::Retry { attempt: retries, wait, error: &e.message() });
                        self.store().set_message_parts(&msg_id, &[])?;
                        (self.cfg.sleep)(wait, cancel);
                        if cancel.load(Ordering::Relaxed) {
                            break r;
                        }
                        continue;
                    }
                    crate::log_warn!("engine", "giving up session={sid} step={step} after {} retries: {}", retries - 1, e.message());
                    break r;
                }
                _ => break r,
            }
        };

        // The provider rejected the prompt as too long before producing anything: compact once and retry the step.
        if let (Err(ProviderError::ContextOverflow(_)), false, false) = (&result.stream, result.dispatched, overflowed) {
            if !cancel.load(Ordering::Relaxed) {
                crate::log_warn!("engine", "context overflow session={sid} step={step}: compacting and retrying once");
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
                let recorded = self.store().add_usage(sid, cost.unwrap_or(0.0), usage);
                self.store_warn(ui, "usage", "token usage and cost could not be recorded, so the session totals will be too low", recorded);
                self.store().usage_totals(sid).map(|t| t.0).unwrap_or(0.0)
            };
            if cost.is_none() {
                self.note_unpriced_for_limits(ui, &format!("{provider_id}/{model_id}"));
            }
            data["cost"] = json!(cost);
            crate::log_debug!("engine", "usage session={sid} step={step} model={provider_id}/{model_id} input={} output={} cache_read={} cache_write={} cost={} session_cost={session_cost:.4}", usage.input, usage.output, usage.cache_read, usage.cache_write, cost.map_or("unpriced".to_string(), |c| format!("{c:.4}")));
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
            Ok(()) if matches!(result.finish, Some((FinishReason::Refusal(_), _))) => {
                // #153: a refusal used to end the turn like a finished answer. Don't continue or retry: the same request
                // would most likely be declined again. What streamed before it stays visible but is incomplete, and
                // `chat.rs` leaves it out of what the model sees next.
                let why = match &result.finish {
                    Some((FinishReason::Refusal(w), _)) if !w.is_empty() => format!(" ({w})"),
                    _ => String::new(),
                };
                let msg = format!("the model declined to continue{why}; anything it wrote before that is incomplete");
                data["finish"] = json!("refusal");
                crate::log_warn!("engine", "refusal session={sid} step={step} model={provider_id}/{model_id}{why}");
                ui.event(&UiEvent::Notice(&msg));
                StepOutcome::Stop(RunOutcome::Failed(msg))
            }
            Ok(()) if result.declined => StepOutcome::Stop(RunOutcome::Declined),
            Ok(()) => {
                if let Some((FinishReason::Other(reason), _)) = &result.finish {
                    if tool_calls == 0 {
                        // A stop reason we don't know ended the turn: say so instead of looking like a normal answer.
                        crate::log_warn!("engine", "unknown stop reason session={sid} step={step} model={provider_id}/{model_id} reason={reason}");
                        ui.event(&UiEvent::Notice(&format!("the response ended with an unexpected stop reason ({reason}), so it may be incomplete")));
                    }
                }
                if matches!(result.finish, Some((FinishReason::MaxTokens, _))) && tool_calls == 0 {
                    let answered = result.parts.iter().any(|p| p["type"] == "text" && p["text"].as_str().is_some_and(|t| !t.trim().is_empty()));
                    StepOutcome::CutOff { answered }
                } else if tool_calls > 0 && tools_disabled {
                    // The step allowance is used up and the model called tools anyway: don't let it go on.
                    let n = agent::effective_steps(&agent, self.cfg.limits.max_steps).unwrap_or(step);
                    StepOutcome::Stop(RunOutcome::LimitReached { what: format!("The step limit ({n}) was reached and the model kept calling tools") })
                } else if tool_calls > 0 {
                    StepOutcome::Continue
                } else {
                    StepOutcome::Done
                }
            }
        };
        self.store().update_message(&msg_id, &data)?;
        // The cost limits: only a step that is followed by another one can be stopped (a finished turn is over anyway).
        let outcome = match outcome {
            StepOutcome::Continue => self.check_limits(sid, ui).map_or(StepOutcome::Continue, StepOutcome::Stop),
            o => o,
        };
        crate::log_debug!("engine", "step end session={sid} step={step} tool_calls={tool_calls} next={}", match &outcome { StepOutcome::Continue => "continue".to_string(), StepOutcome::Done => "done".to_string(), StepOutcome::CutOff { answered } => format!("cut-off answered={answered}"), StepOutcome::Stop(o) => format!("stop:{o:?}") });
        Ok(outcome)
    }

    /// One streaming request. Tools start executing as soon as their call is complete.
    #[allow(clippy::too_many_arguments)]
    fn attempt(&self, provider: &dyn Provider, session: &Session, req: &Request, permitter: &EnginePermitter, ui: &dyn Ui, cancel: &AtomicBool, msg_id: &str, caps: MediaCaps, step_tools: &[Arc<dyn Tool>], checks_on: bool) -> AttemptResult {
        let cwd = PathBuf::from(&session.directory);
        let root = project_root_of(&cwd);
        let spill = self.spill_dir();
        let mut parts: Vec<Value> = vec![];
        let mut text_idx: Option<usize> = None;
        let mut finish = None;
        let mut dispatched = false;
        let mut declined = false;
        let mut bad_calls: Vec<(usize, String)> = vec![];

        let stream = std::thread::scope(|s| {
            let mut handles = vec![];
            let res = provider.stream(req, cancel, &mut |ev| match ev {
                Event::TextDelta(t) => {
                    ui.event(&UiEvent::TextDelta(&t));
                    let i = *text_idx.get_or_insert_with(|| {
                        parts.push(json!({"type":"text","text":""}));
                        parts.len() - 1
                    });
                    // Append in place: copying the text so far on every delta would be quadratic.
                    if let Value::String(s) = &mut parts[i]["text"] {
                        s.push_str(&t);
                    }
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
                    // Claude Code's argument names become ours before anything sees the call (see `normalize_input`).
                    let input = tools::normalize_input(&name, input);
                    ui.event(&UiEvent::ToolStarted { id: &id, name: &name, input: &input });
                    parts.push(chat::tool_part(&id, &name, &input));
                    let idx = parts.len() - 1;
                    let saved = self.store().set_message_parts(msg_id, &parts);
                    self.store_warn(ui, "parts", "a tool call could not be saved, so this session may not resume cleanly after a crash", saved);
                    let tool = step_tools.iter().find(|t| t.name() == name).map(|t| t.as_ref());
                    let (cwd, root, spill) = (cwd.clone(), root.clone(), spill.clone());
                    handles.push((
                        idx,
                        s.spawn(move || {
                            let started = std::time::Instant::now();
                            let outcome = match tool {
                                None => Err(ToolError::fail(format!("Unknown tool: {name}"))),
                                Some(t) => {
                                    let host = EngineHost { engine: self, session, ui, cancel };
                                    t.run(&input, &ToolCtx { formatters: &self.cfg.formatters, media: caps, session_id: &session.id, host: &host, cwd, project_root: root, spill_dir: spill, permitter, cancel, call_id: &id })
                                }
                            };
                            log_tool(&name, &outcome, started.elapsed());
                            outcome
                        }),
                    ));
                }
                Event::BadToolCall { id, name, raw, error } => {
                    text_idx = None;
                    dispatched = true;
                    // Nothing runs; the model gets a tool error (written once the finish reason is known).
                    let input = crate::provider::bad_call_input(&raw);
                    ui.event(&UiEvent::ToolStarted { id: &id, name: &name, input: &input });
                    parts.push(chat::tool_part(&id, &name, &input));
                    bad_calls.push((parts.len() - 1, error));
                    let saved = self.store().set_message_parts(msg_id, &parts);
                    self.store_warn(ui, "parts", "a tool call could not be saved, so this session may not resume cleanly after a crash", saved);
                }
                Event::Finish { reason, usage } => finish = Some((reason, usage)),
            });
            let max_tokens = matches!(finish, Some((FinishReason::MaxTokens, _)));
            for (idx, error) in bad_calls {
                let name = parts[idx]["name"].as_str().unwrap_or("").to_string();
                let msg = bad_call_message(&name, &error, max_tokens);
                let state = self.finish_tool(&name, Err(ToolError::fail(msg)), &spill, ui);
                parts[idx]["state"] = state;
            }
            let mut finished = vec![];
            for (idx, h) in handles {
                let outcome = h.join().unwrap_or_else(|_| Err(ToolError::fail("tool panicked")));
                declined |= outcome == Err(ToolError::Declined);
                let name = parts[idx]["name"].as_str().unwrap_or("").to_string();
                if let (true, Ok(out)) = (name == "read", &outcome) {
                    // Reading a file pulls in AGENTS.md files between it and the working directory.
                    if let Some(p) = out.metadata["path"].as_str() {
                        for ins in instructions::discover_between(std::path::Path::new(p), &cwd) {
                            let added = self.store().add_instruction(&session.id, &ins.to_string_lossy());
                            self.store_warn(ui, "instruction", "an AGENTS.md file the model read could not be remembered, so its instructions may be dropped on the next step", added);
                        }
                    }
                }
                finished.push((idx, name, outcome));
            }
            // Once per step, after the whole batch: the checks see every file the step changed.
            if checks_on {
                self.run_checks(&session.id, &cwd, cancel, &mut finished);
            }
            for (idx, name, outcome) in finished {
                let state = self.finish_tool(&name, outcome, &spill, ui);
                parts[idx]["state"] = state;
            }
            res
        });
        AttemptResult { parts, stream, finish, dispatched, declined }
    }

    /// Run the configured checks on the files this step's edit, write and patch calls changed, and add what they
    /// report to the output of the last tool that changed a file the check covers. Never fails a tool.
    fn run_checks(&self, sid: &str, cwd: &std::path::Path, cancel: &AtomicBool, finished: &mut [(usize, String, Result<ToolOutput, ToolError>)]) {
        if self.cfg.checks.is_empty() || cancel.load(Ordering::Relaxed) {
            return;
        }
        // Each changed file, with the position (in `finished`) of the last call that changed it.
        let mut changed: Vec<(PathBuf, usize)> = vec![];
        for (pos, (_, name, outcome)) in finished.iter().enumerate() {
            let Ok(out) = outcome else { continue };
            for p in changed_files(name, out, cwd) {
                match changed.iter_mut().find(|(f, _)| *f == p) {
                    Some(e) => e.1 = pos,
                    None => changed.push((p, pos)),
                }
            }
        }
        if changed.is_empty() {
            return;
        }
        let files: Vec<PathBuf> = changed.iter().map(|(p, _)| p.clone()).collect();
        for run in self.cfg.checks.run(&files, cwd, cancel) {
            let key = format!("{sid}\0{}", run.name);
            let was_failing = {
                let mut failing = lock(&self.check_failing);
                let was = failing.contains(&key);
                match run.verdict {
                    crate::check::Verdict::Errors(_) => failing.insert(key),
                    crate::check::Verdict::Passed => failing.remove(&key),
                    _ => false,
                };
                was
            };
            crate::log_debug!("check", "{} files={} verdict={}", run.name, run.files.len(), match &run.verdict { crate::check::Verdict::Passed => "passed", crate::check::Verdict::Errors(_) => "errors", crate::check::Verdict::Note(_) => "note", crate::check::Verdict::Cancelled => "cancelled" });
            let Some(text) = crate::check::note(&run, was_failing) else { continue };
            let pos = changed.iter().filter(|(p, _)| run.files.contains(p)).map(|(_, pos)| *pos).max();
            if let Some(Ok(out)) = pos.and_then(|i| finished.get_mut(i)).map(|f| &mut f.2) {
                out.text.push_str(&format!("\n\n{text}"));
            }
        }
    }

    fn finish_tool(&self, name: &str, outcome: Result<ToolOutput, ToolError>, spill: &std::path::Path, ui: &dyn Ui) -> Value {
        match outcome {
            Ok(mut out) => {
                // Images and PDFs a tool returned go to the blob store; the message keeps a small reference.
                let mut media = vec![];
                for a in out.take_media() {
                    match blobs::put(&self.cfg.app_dir, &a.bytes) {
                        Ok(hash) => media.push(json!({"mediaType": a.media_type, "blob": hash, "size": a.bytes.len(), "name": a.name})),
                        Err(e) => {
                            crate::log_warn!("tool", "{name}: could not store the attachment {}: {e}", a.name);
                            out.text.push_str(&format!("\n[could not store the attachment {}: {e}]", a.name))
                        }
                    }
                }
                let text = match crate::truncate::truncate(&out.text, spill) {
                    Ok(t) => {
                        if let Some(p) = &t.output_path {
                            crate::log_debug!("tool", "{name} output truncated; the full text is in {}", p.display());
                        }
                        t.text
                    }
                    Err(e) => {
                        crate::log_warn!("tool", "{name}: could not save the long output to {}: {e}", spill.display());
                        out.text
                    }
                };
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

/// The files an edit, write or patch call changed (not deleted), as absolute paths.
fn changed_files(tool: &str, out: &ToolOutput, cwd: &std::path::Path) -> Vec<PathBuf> {
    let abs = |p: &str| if std::path::Path::new(p).is_absolute() { PathBuf::from(p) } else { cwd.join(p) };
    match tool {
        "edit" | "write" => out.metadata["path"].as_str().map(abs).into_iter().collect(),
        "patch" => out.metadata["files"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).filter_map(|l| l.split_once(' ')).filter(|(op, _)| *op != "D").map(|(_, p)| abs(p)).collect(),
        _ => vec![],
    }
}

/// The tool error a model gets for a call whose input was cut off or was not valid JSON. Nothing was run.
fn bad_call_message(name: &str, error: &str, max_tokens: bool) -> String {
    let cause = if max_tokens {
        "The response hit the output token limit (max_tokens) while writing it, so the input was cut off."
    } else {
        "The input was probably cut off by the output token limit, or it was malformed."
    };
    format!(
        "The input for the {name} tool call was not valid JSON ({error}), so nothing was run. {cause} \
         Retry with a smaller input: write large files in smaller parts (create the file, then add to it with edit)."
    )
}
