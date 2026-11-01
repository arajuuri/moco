//! The engine's side of the command classifier (`classifier.rs`): choosing the model, calling it with a time limit,
//! recording its cost and remembering its allows.
use super::*;
use crate::classifier::{self, Settings, Verdict};
use crate::tools::shellparse::Parsed;
use std::sync::mpsc;
use std::time::Instant;

/// What the permitter needs to consult the classifier for one step.
pub(super) struct ClassifyCtx<'a> {
    pub engine: &'a Engine,
    pub session: &'a Session,
    pub settings: &'a Settings,
    pub cancel: &'a AtomicBool,
}

/// Why a classifier call produced no reply. An interrupt is the user's doing, not a failure of the classifier, and
/// no decision was made, so it is told apart from the rest.
enum CallError {
    Interrupted,
    Failed(String),
}

/// The outcome of `Engine::evaluate_command`: which layer decided, and what it said.
pub struct Evaluation {
    /// `rules` (never sent to the model), `model`, or `off` (no classifier configured).
    pub stage: &'static str,
    pub verdict: Verdict,
}

/// A front end that shows nothing and refuses every prompt: the evaluation has no user.
struct NullUi;

impl Ui for NullUi {
    fn event(&self, _: &UiEvent) {}
    fn ask(&self, _: &PermissionPrompt) -> Reply {
        Reply::Reject(None)
    }
}

impl Engine {
    /// Run one command through the layers that come before asking the user (the never-classify rules, then the model),
    /// the way `auto_allowed` does, for `moco --eval-classifier` and `scripts/eval-classifier.py`. Explicit permission
    /// rules are not consulted: they are the user's, not part of what is being measured.
    pub fn evaluate_command(&self, session: &Session, command: &str, cancel: &AtomicBool) -> Evaluation {
        let parsed = crate::tools::shellparse::parse(command);
        if classifier::never_classify(&parsed) {
            return Evaluation { stage: "rules", verdict: Verdict::Ask { reason: "opaque, or on the never-classify list".into() } };
        }
        let Some(settings) = &self.cfg.classifier else { return Evaluation { stage: "off", verdict: Verdict::Ask { reason: "the classifier is off".into() } } };
        Evaluation { stage: "model", verdict: self.classify_command(session, command, &parsed, settings, &NullUi, cancel) }
    }

    /// The model for small background jobs: `small_model`, else the session's own model.
    pub(super) fn small_model_ref(&self, session: &Session) -> String {
        let agent = agent::find(self.agents_of(session), &session.agent).or_else(|| self.agents_of(session).first().cloned());
        self.cfg.small_model.clone().or(session.model.clone()).or(agent.and_then(|a| a.model)).unwrap_or_else(|| self.cfg.model.clone())
    }

    fn provider_for(&self, provider_id: &str) -> Option<&dyn Provider> {
        self.providers.get(provider_id).map(|p| p.as_ref()).or(self.fallback.as_deref())
    }

    /// Ask the classifier about `command`. Anything that goes wrong (no model, no provider, timeout, error, garbled
    /// reply) is an `Ask`: the user decides. Only allows are remembered, for this session and directory. Every
    /// decision is logged (target `classifier`, on at `info` unless the log filter says otherwise).
    pub(super) fn classify_command(&self, session: &Session, command: &str, parsed: &Parsed, settings: &Settings, ui: &dyn Ui, cancel: &AtomicBool) -> Verdict {
        let commands: Vec<String> = parsed.commands.iter().map(|c| c.text.clone()).collect();
        let key = format!("{}\u{0}{}\u{0}{}", session.id, session.directory, if commands.is_empty() { command.trim().to_string() } else { commands.join(" ; ") });
        let shown = classifier::sanitize(command, 200);
        if self.classified.lock().unwrap_or_else(|e| e.into_inner()).contains(&key) {
            crate::log_info!("classifier", "session={} decision=allow cached=true command={shown}", session.id);
            return Verdict::Allow { risk: "allowed earlier".into(), reason: String::new() };
        }
        let model_ref = settings.model.clone().unwrap_or_else(|| self.small_model_ref(session));
        let (provider_id, model_id) = config::split_model(&model_ref);
        if self.provider_for(provider_id).is_none() {
            crate::log_info!("classifier", "session={} decision=ask model={model_ref} reason=provider not connected command={shown}", session.id);
            return Verdict::Ask { reason: format!("the classifier's provider '{provider_id}' is not connected") };
        }
        let root = project_root_of(std::path::Path::new(&session.directory));
        let req = Request {
            model: model_id.to_string(),
            system: classifier::SYSTEM.into(),
            messages: vec![ChatMessage { role: Role::User, content: vec![Block::Text(classifier::request_text(command, &commands, &session.directory, &root.to_string_lossy()))] }],
            tools: vec![],
            max_tokens: 200,
            overlay: self.base_overlay(provider_id, model_id),
            ..Default::default()
        };
        let started = Instant::now();
        let (reply, usage) = match self.call_classifier(provider_id, req, settings.timeout, cancel) {
            Ok(r) => r,
            Err(CallError::Interrupted) => {
                // Esc while the classifier was thinking: nothing was decided, so no decision line and no message.
                crate::log_debug!("classifier", "session={} interrupted after {} ms, no decision made command={shown}", session.id, started.elapsed().as_millis());
                return Verdict::Ask { reason: String::new() };
            }
            Err(CallError::Failed(why)) => {
                crate::log_info!("classifier", "session={} decision=ask model={model_ref} ms={} reason={why} command={shown}", session.id, started.elapsed().as_millis());
                return Verdict::Ask { reason: "the classifier did not answer in time".into() };
            }
        };
        let ms = started.elapsed().as_millis();
        let priced = self.cfg.catalog.model(provider_id, model_id).and_then(|m| catalog::cost(m, &usage));
        let cost = priced.unwrap_or(0.0);
        let (session_cost, classifier_cost) = {
            let st = self.store();
            st.add_classifier_usage(&session.id, cost, &usage).log_err("store", "recording classifier usage");
            (st.usage_totals(&session.id).map(|t| t.0).unwrap_or(0.0), st.classifier_cost(&session.id).unwrap_or(0.0))
        };
        // Shown at once: the run may end (a rejection, an interrupt) before another step reports the total.
        ui.event(&UiEvent::ClassifierUsage { cost, session_cost, classifier_cost });
        if priced.is_none() && self.unpriced_noted.lock().unwrap_or_else(|e| e.into_inner()).insert(model_ref.clone()) {
            ui.event(&UiEvent::Notice(&format!("the classifier model {model_ref} has no price in the catalog, so its cost is counted as $0.00 (set a price under providers.<id>.models, or pick a model with one)")));
        }
        let verdict = classifier::parse_reply(&reply);
        let (said, risk) = classifier::reply_fields(&reply);
        let (decision, why) = match &verdict {
            Verdict::Allow { reason, .. } => ("allow", reason.as_str()),
            Verdict::Ask { reason } => ("ask", reason.as_str()),
        };
        crate::log_info!(
            "classifier",
            "session={} decision={decision} model_verdict={} risk={} model={model_ref} ms={ms} tokens_in={} tokens_out={} cost={cost:.5} reason={why} command={shown}",
            session.id,
            if said.is_empty() { "-" } else { &said },
            if risk.is_empty() { "-" } else { &risk },
            usage.input + usage.cache_read + usage.cache_write,
            usage.output
        );
        crate::log_debug!("classifier", "session={} raw reply: {}", session.id, classifier::sanitize(&reply, 500));
        if let Verdict::Allow { risk, reason } = &verdict {
            self.classified.lock().unwrap_or_else(|e| e.into_inner()).insert(key);
            self.store().record_auto_allow(&session.id, command, risk, reason, &model_ref).log_err("store", "recording an automatic allow");
        }
        verdict
    }

    /// One non-streaming call with a time limit. When the engine is shared the call runs on its own thread, so a
    /// stalled connection cannot hold up the tool past the limit; otherwise it runs inline.
    fn call_classifier(&self, provider_id: &str, req: Request, timeout: Duration, cancel: &AtomicBool) -> Result<(String, Usage), CallError> {
        fn collect(provider: Option<&dyn Provider>, req: &Request, stop: &AtomicBool) -> Result<(String, Usage), CallError> {
            let provider = provider.ok_or_else(|| CallError::Failed("no provider".into()))?;
            let (mut out, mut usage) = (String::new(), Usage::default());
            provider
                .stream(req, stop, &mut |ev| match ev {
                    Event::TextDelta(t) => out.push_str(&t),
                    Event::Finish { usage: u, .. } => usage = u,
                    _ => {}
                })
                .map_err(|e| match e {
                    ProviderError::Interrupted => CallError::Interrupted,
                    e => CallError::Failed(format!("provider error: {}", e.message())),
                })?;
            Ok((out, usage))
        }
        let Ok(me) = self.shared() else { return collect(self.provider_for(provider_id), &req, cancel) };
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (stop2, pid) = (stop.clone(), provider_id.to_string());
        std::thread::spawn(move || {
            let _ = tx.send(collect(me.provider_for(&pid), &req, &stop2));
        });
        let end = Instant::now() + timeout;
        loop {
            match rx.recv_timeout(Duration::from_millis(25)) {
                Ok(r) => return r,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(CallError::Failed("the classifier thread ended without an answer".into())),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if cancel.load(Ordering::Relaxed) {
                        stop.store(true, Ordering::Relaxed);
                        return Err(CallError::Interrupted);
                    }
                    if Instant::now() >= end {
                        stop.store(true, Ordering::Relaxed);
                        return Err(CallError::Failed(format!("no answer within {} ms", timeout.as_millis())));
                    }
                }
            }
        }
    }
}
