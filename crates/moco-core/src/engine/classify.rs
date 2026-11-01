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

impl Engine {
    /// The model for small background jobs: `small_model`, else the session's own model.
    pub(super) fn small_model_ref(&self, session: &Session) -> String {
        let agent = agent::find(self.agents_of(session), &session.agent).or_else(|| self.agents_of(session).first().cloned());
        self.cfg.small_model.clone().or(session.model.clone()).or(agent.and_then(|a| a.model)).unwrap_or_else(|| self.cfg.model.clone())
    }

    fn provider_for(&self, provider_id: &str) -> Option<&dyn Provider> {
        self.providers.get(provider_id).map(|p| p.as_ref()).or(self.fallback.as_deref())
    }

    /// Ask the classifier about `command`. Anything that goes wrong (no model, no provider, timeout, error, garbled
    /// reply) is an `Ask`: the user decides. Only allows are remembered, for this session and directory.
    pub(super) fn classify_command(&self, session: &Session, command: &str, parsed: &Parsed, settings: &Settings, ui: &dyn Ui, cancel: &AtomicBool) -> Verdict {
        let commands: Vec<String> = parsed.commands.iter().map(|c| c.text.clone()).collect();
        let key = format!("{}\u{0}{}\u{0}{}", session.id, session.directory, if commands.is_empty() { command.trim().to_string() } else { commands.join(" ; ") });
        if self.classified.lock().unwrap_or_else(|e| e.into_inner()).contains(&key) {
            return Verdict::Allow { risk: "allowed earlier".into(), reason: String::new() };
        }
        let model_ref = settings.model.clone().unwrap_or_else(|| self.small_model_ref(session));
        let (provider_id, model_id) = config::split_model(&model_ref);
        if self.provider_for(provider_id).is_none() {
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
        let Some((reply, usage)) = self.call_classifier(provider_id, req, settings.timeout, cancel) else {
            return Verdict::Ask { reason: "the classifier did not answer in time".into() };
        };
        let priced = self.cfg.catalog.model(provider_id, model_id).and_then(|m| catalog::cost(m, &usage));
        let cost = priced.unwrap_or(0.0);
        let (session_cost, classifier_cost) = {
            let st = self.store();
            let _ = st.add_classifier_usage(&session.id, cost, &usage);
            (st.usage_totals(&session.id).map(|t| t.0).unwrap_or(0.0), st.classifier_cost(&session.id).unwrap_or(0.0))
        };
        // Shown at once: the run may end (a rejection, an interrupt) before another step reports the total.
        ui.event(&UiEvent::ClassifierUsage { cost, session_cost, classifier_cost });
        if priced.is_none() && self.unpriced_noted.lock().unwrap_or_else(|e| e.into_inner()).insert(model_ref.clone()) {
            ui.event(&UiEvent::Notice(&format!("the classifier model {model_ref} has no price in the catalog, so its cost is counted as $0.00 (set a price under providers.<id>.models, or pick a model with one)")));
        }
        let verdict = classifier::parse_reply(&reply);
        if matches!(verdict, Verdict::Allow { .. }) {
            self.classified.lock().unwrap_or_else(|e| e.into_inner()).insert(key);
        }
        verdict
    }

    /// One non-streaming call with a time limit. When the engine is shared the call runs on its own thread, so a
    /// stalled connection cannot hold up the tool past the limit; otherwise it runs inline.
    fn call_classifier(&self, provider_id: &str, req: Request, timeout: Duration, cancel: &AtomicBool) -> Option<(String, Usage)> {
        fn collect(provider: &dyn Provider, req: &Request, stop: &AtomicBool) -> Option<(String, Usage)> {
            let (mut out, mut usage) = (String::new(), Usage::default());
            provider
                .stream(req, stop, &mut |ev| match ev {
                    Event::TextDelta(t) => out.push_str(&t),
                    Event::Finish { usage: u, .. } => usage = u,
                    _ => {}
                })
                .ok()?;
            Some((out, usage))
        }
        let Ok(me) = self.shared() else { return collect(self.provider_for(provider_id)?, &req, cancel) };
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (stop2, pid) = (stop.clone(), provider_id.to_string());
        std::thread::spawn(move || {
            let _ = tx.send(me.provider_for(&pid).and_then(|p| collect(p, &req, &stop2)));
        });
        let end = Instant::now() + timeout;
        loop {
            match rx.recv_timeout(Duration::from_millis(25)) {
                Ok(r) => return r,
                Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if cancel.load(Ordering::Relaxed) || Instant::now() >= end {
                        stop.store(true, Ordering::Relaxed);
                        return None;
                    }
                }
            }
        }
    }
}
