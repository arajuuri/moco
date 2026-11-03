//! Session titles from a small model.
use super::*;

impl Engine {
    pub(super) fn title_job(&self, sid: &str) -> Option<(Session, String)> {
        let session = self.store().session(sid).ok()??;
        if session.title.is_some() || session.parent_id.is_some() {
            return None;
        }
        let (first, latest) = self.store().title_source(sid).ok()??;
        if !self.title_tried.lock().unwrap().insert(sid.to_string()) {
            return None;
        }
        Some((session, crate::title::request_text(&first, latest.as_deref())))
    }

    /// Best effort: any failure just leaves the session untitled. Only errors where the request provably never ran
    /// (connection refused, DNS) are retried, up to 3 times, since this is a one-shot generation.
    pub(super) fn generate_title(&self, session: &Session, text: &str, ui: &dyn Ui, cancel: &AtomicBool) {
        let model_ref = self.small_model_ref(session);
        let (provider_id, model_id) = config::split_model(&model_ref);
        let Some(provider) = self.providers.get(provider_id).map(|p| p.as_ref()).or(self.fallback.as_deref()) else { return };
        let (effort, max_tokens) = self.short_answer_shape(provider_id, model_id, 60);
        let req = Request {
            model: model_id.to_string(),
            system: crate::title::SYSTEM.into(),
            messages: vec![ChatMessage { role: Role::User, content: vec![Block::Text(text.to_string())] }],
            tools: vec![],
            max_tokens,
            effort,
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
        let Some(title) = crate::title::clean(&reply) else { return };
        let st = self.store();
        if st.set_title_if_empty(&session.id, &title).unwrap_or(false) {
            let cost = self.cfg.catalog.model(provider_id, model_id).and_then(|m| catalog::cost(m, &usage)).unwrap_or(0.0);
            st.add_usage(&session.id, cost, &usage).log_err("store", "recording title usage");
            drop(st);
            ui.event(&UiEvent::Titled(&title));
        }
    }
}
