//! Manual and automatic compaction.
use super::*;

pub(super) fn human_tokens(n: u64) -> String {
    if n >= 10_000 { format!("{}k", n / 1000) } else { n.to_string() }
}

impl Engine {
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
    pub(super) fn compact_inner(&self, sid: &str, trigger: &str, ui: &dyn Ui, cancel: &AtomicBool) -> Result<CompactResult, StoreError> {
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
        crate::log_info!("engine", "compaction start session={sid} trigger={trigger} replacing={k} messages, about {before} tokens, model={provider_id}/{model_id}");
        let compaction_started = std::time::Instant::now();
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
            crate::log_warn!("engine", "compaction failed session={sid} trigger={trigger}: {e}");
            return Ok(CompactResult::Failed(e));
        }
        let summary = reply.trim().to_string();
        if summary.is_empty() {
            crate::log_warn!("engine", "compaction failed session={sid} trigger={trigger}: the model returned an empty summary");
            return Ok(CompactResult::Failed("the model returned an empty summary".into()));
        }
        let after: u64 = compaction::estimate_text(&summary) + eff[k..].iter().map(compaction::estimate).sum::<u64>();
        {
            let st = self.store();
            st.add_message(sid, "compaction", &json!({ "summary": summary, "keepFrom": keep_from, "trigger": trigger, "replaced": k }))?;
            let cost = self.cfg.catalog.model(&provider_id, &model_id).and_then(|m| catalog::cost(m, &usage_total)).unwrap_or(0.0);
            st.add_usage(sid, cost, &usage_total)?;
        }
        crate::log_info!("engine", "compaction done session={sid} trigger={trigger} tokens {before} -> {after} ms={}", compaction_started.elapsed().as_millis());
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
                            crate::log_warn!("engine", "retry summary request attempt={attempt} wait_ms={}: {}", wait.as_millis(), e.message());
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
}
