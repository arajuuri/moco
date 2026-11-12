//! Manual and automatic compaction.
use super::*;

pub(super) fn human_tokens(n: u64) -> String {
    if n >= 10_000 { format!("{}k", n / 1000) } else { n.to_string() }
}

/// How many messages to drop, oldest first from `first`, until the total size fits `budget`. Always keeps at least
/// one message after `first`.
fn trim_count(sizes: &[u64], first: usize, budget: u64) -> usize {
    let mut total: u64 = sizes.iter().sum();
    let mut n = 0;
    while sizes.len() - n > first + 1 && total > budget {
        total -= sizes[first + n];
        n += 1;
    }
    n
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
            (st.session(sid)?.ok_or(StoreError::SessionGone)?, st.messages(sid)?)
        };
        self.compact_with(sid, trigger, session, &history, ui, cancel)
    }

    /// `compact_inner` on a session and history the caller has just loaded (the step loop's `prepare` did, so
    /// auto-compaction does not parse the whole history a second time).
    pub(super) fn compact_with(&self, sid: &str, trigger: &str, session: Session, history: &[crate::store::Message], ui: &dyn Ui, cancel: &AtomicBool) -> Result<CompactResult, StoreError> {
        let agent = agent::find(self.agents_of(&session), &session.agent).or_else(|| self.agents_of(&session).first().cloned()).expect("an agent");
        let (provider, provider_id, model_id) = match self.resolve_model(&session, &agent) {
            Ok(r) => r,
            Err(e) => return Ok(CompactResult::Failed(e)),
        };
        let eff = compaction::effective(history);
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
            let sizes: Vec<u64> = portion.iter().map(compaction::estimate).collect();
            let n = trim_count(&sizes, first_droppable, l * 8 / 10);
            portion.drain(first_droppable..first_droppable + n);
        }
        let mut reply = String::new();
        let mut usage_total = Usage::default();
        let mut failure = None;
        'attempts: for _ in 0..4 {
            let mut messages = self.chat(&portion);
            append_user_text(&mut messages, compaction::prompt(update));
            let mut req = Request { model: model_id.clone(), system: compaction::SYSTEM.into(), messages, overlay: self.base_overlay(&provider_id, &model_id), ..Default::default() };
            req.max_tokens = fit_output(self.output_budget(&provider_id, &model_id), limit, compaction::estimate_request(&req));
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
        let session_cost = {
            let st = self.store();
            st.add_message(sid, "compaction", &json!({ "summary": summary, "keepFrom": keep_from, "trigger": trigger, "replaced": k }))?;
            let cost = self.cfg.catalog.model(&provider_id, &model_id).and_then(|m| catalog::cost(m, &usage_total)).unwrap_or(0.0);
            st.add_usage(sid, cost, &usage_total)?;
            st.usage_totals(sid)?.0
        };
        crate::log_info!("engine", "compaction done session={sid} trigger={trigger} tokens {before} -> {after} ms={}", compaction_started.elapsed().as_millis());
        ui.event(&UiEvent::Status(&format!("compacted: about {} -> {} tokens", human_tokens(before), human_tokens(after))));
        ui.event(&UiEvent::ContextUsage { context_tokens: after, context_limit: limit, session_cost });
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

#[cfg(test)]
mod tests {
    use super::trim_count;

    /// The original remove loop, kept as the reference.
    fn reference(sizes: &[u64], first: usize, budget: u64) -> usize {
        let mut v = sizes.to_vec();
        let mut n = 0;
        while v.len() > first + 1 && v.iter().sum::<u64>() > budget {
            v.remove(first);
            n += 1;
        }
        n
    }

    #[test]
    fn trim_matches_the_remove_loop() {
        let cases: &[(&[u64], usize, u64, usize)] = &[
            (&[10, 10, 10, 10], 0, 100, 0),
            (&[10, 10, 10, 10], 0, 25, 2),
            (&[10, 10, 10, 10], 0, 0, 3),
            (&[50, 10, 10, 10], 1, 20, 2),
            (&[50, 10, 10, 10], 1, 0, 2),
            (&[5], 0, 0, 0),
            (&[5, 5], 1, 0, 0),
            (&[], 0, 0, 0),
            (&[30, 1, 1, 1], 0, 3, 1),
        ];
        for (sizes, first, budget, want) in cases {
            assert_eq!(trim_count(sizes, *first, *budget), *want, "{sizes:?} {first} {budget}");
            assert_eq!(reference(sizes, *first, *budget), *want);
        }
    }
}
