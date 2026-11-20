//! The suggested next message after a turn (issue archive#66; `suggest.rs` has the prompt). Asked for by the TUI.
use super::*;

impl Engine {
    /// One short message the user might send next in session `sid`, from the small model, or `None`. Best effort:
    /// any failure, a cancel, or a model that sees no clear next step gives `None`. The cost is added to the session.
    pub fn suggest_next(&self, sid: &str, cancel: &AtomicBool) -> Option<String> {
        let (session, prompt, todos) = {
            let st = self.store();
            let session = st.session(sid).ok()??;
            let prompt = st.messages(sid).ok()?.iter().rev().filter(|m| m.kind == "user").find_map(|m| m.data["text"].as_str().filter(|t| !t.trim().is_empty()).map(String::from))?;
            let todos: Vec<String> = st.todos(sid).unwrap_or_default().into_iter().filter(|t| t.status != "completed" && t.status != "cancelled").map(|t| t.content).collect();
            (session, prompt, todos)
        };
        let reply = self.last_assistant_text(sid)?;
        let model_ref = self.small_model_ref(&session);
        let (provider_id, model_id) = config::split_model(&model_ref);
        let provider = self.providers.get(provider_id).map(|p| p.as_ref()).or(self.fallback.as_deref())?;
        let (effort, max_tokens) = self.short_answer_shape(provider_id, model_id, 40);
        let req = Request {
            model: model_id.to_string(),
            system: crate::suggest::SYSTEM.into(),
            messages: vec![ChatMessage { role: Role::User, content: vec![Block::Text(crate::suggest::request_text(&prompt, &reply, &todos))] }],
            tools: vec![],
            max_tokens,
            effort,
            overlay: self.base_overlay(provider_id, model_id),
            ..Default::default()
        };
        let (mut out, mut usage) = (String::new(), Usage::default());
        let r = provider.stream(&req, cancel, &mut |ev| match ev {
            Event::TextDelta(t) => out.push_str(&t),
            Event::Finish { usage: u, .. } => usage = u,
            _ => {}
        });
        if usage != Usage::default() {
            let cost = self.cfg.catalog.model(provider_id, model_id).and_then(|m| catalog::cost(m, &usage)).unwrap_or(0.0);
            self.store().add_usage(sid, cost, &usage).log_err("store", "recording suggestion usage");
        }
        if let Err(e) = r {
            crate::log_debug!("engine", "session={sid} next-action suggestion failed: {e:?}");
            return None;
        }
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        crate::suggest::clean(&out)
    }
}
