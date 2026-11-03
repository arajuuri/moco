//! Spending guardrails (config `limits`, `--max-cost`, `--max-steps`).
//!
//! The step limit is applied where the request is built (`step.rs`, `agent::effective_steps`) and reuses the
//! `agent::MAX_STEPS` reminder. The cost limits are checked after every step that goes on to another step:
//!
//! - `maxCostPerRun` compares the spend of the top-level session (which includes its subagents, see
//!   `Store::add_usage`) with what it was when the run started or when new input arrived. A subagent that runs
//!   inside a run is measured against the run it belongs to.
//! - `maxCostPerSession` compares the top-level session's total spend with the limit.
//!
//! When a limit is passed, an attended UI is asked (continue, continue without the limit, stop). An unattended one
//! (a background subagent) or `-p` stops the run with `RunOutcome::LimitReached`.
use super::*;

/// Start of the error a parent gets from a subagent that was stopped by a limit (background jobs report it apart).
pub(super) const SUBAGENT_STOPPED: &str = "Subagent stopped at a spending limit: ";

#[derive(Default)]
pub(super) struct State {
    /// Spend of the session at the start of each active run (`f64::INFINITY` once the user lifted the limit).
    runs: Mutex<HashMap<String, f64>>,
    /// Spend that no longer counts toward `maxCostPerSession`, per top-level session.
    session_floor: Mutex<HashMap<String, f64>>,
}

/// Removes the run's baseline when the run ends, also by panic.
pub(super) struct RunBudget<'a> {
    engine: &'a Engine,
    sid: String,
}

impl Drop for RunBudget<'_> {
    fn drop(&mut self) {
        lock(&self.engine.limit_state.runs).remove(&self.sid);
    }
}

fn usd(x: f64) -> String {
    if x >= 1.0 {
        format!("${x:.2}")
    } else if x >= 0.001 {
        format!("${x:.4}")
    } else {
        format!("${x:.6}")
    }
}

impl Engine {
    /// The configured spending limits (the front end shows them next to the cost).
    pub fn limits(&self) -> crate::config::Limits {
        self.cfg.limits
    }

    /// The session and its ancestors, the top-level session first.
    fn lineage(&self, sid: &str) -> Vec<String> {
        let st = self.store();
        let mut v = vec![sid.to_string()];
        while v.len() < 16 {
            match st.session(v.last().map(String::as_str).unwrap_or(sid)).ok().flatten().and_then(|s| s.parent_id) {
                Some(p) => v.push(p),
                None => break,
            }
        }
        v.reverse();
        v
    }

    fn spent_by(&self, sid: &str) -> f64 {
        self.store().usage_totals(sid).map(|t| t.0).unwrap_or(0.0)
    }

    /// Note what the session has cost so far as the start of a run. A run inside another run (a subagent) shares the
    /// outer run's baseline, so nothing is recorded for it.
    pub(super) fn limits_begin(&self, sid: &str) -> Option<RunBudget<'_>> {
        self.cfg.limits.max_cost_per_run?;
        let line = self.lineage(sid);
        let base = self.spent_by(sid);
        let mut runs = lock(&self.limit_state.runs);
        if line.iter().any(|s| runs.contains_key(s)) {
            return None;
        }
        runs.insert(sid.to_string(), base);
        Some(RunBudget { engine: self, sid: sid.to_string() })
    }

    /// New input starts a new allowance.
    pub(super) fn limits_new_input(&self, sid: &str) {
        if self.cfg.limits.max_cost_per_run.is_none() {
            return;
        }
        let base = self.spent_by(sid);
        if let Some(b) = lock(&self.limit_state.runs).get_mut(sid) {
            *b = base;
        }
    }

    fn ask_to_continue(&self, ui: &dyn Ui, what: &str) -> LimitChoice {
        if ui.unattended() {
            return LimitChoice::Stop;
        }
        let _one_prompt_at_a_time = lock(&self.prompt_lock);
        ui.ask_limit(what)
    }

    /// After a step that is followed by another one: `Some` when a cost limit stops the run.
    pub(super) fn check_limits(&self, sid: &str, ui: &dyn Ui) -> Option<RunOutcome> {
        let l = self.cfg.limits;
        if !l.has_cost_limit() {
            return None;
        }
        let line = self.lineage(sid);
        if let Some(max) = l.max_cost_per_run {
            let owner = {
                let runs = lock(&self.limit_state.runs);
                line.iter().find(|s| runs.contains_key(*s)).cloned()
            };
            let base = owner.as_ref().and_then(|o| lock(&self.limit_state.runs).get(o).copied());
            if let (Some(owner), Some(base)) = (owner, base) {
                let spent = self.spent_by(&owner) - base;
                if spent > max {
                    let what = format!("This turn has cost {} (limit {})", usd(spent), usd(max));
                    crate::log_warn!("engine", "limit reached session={sid}: {what}");
                    let next = match self.ask_to_continue(ui, &what) {
                        LimitChoice::Stop => return Some(RunOutcome::LimitReached { what }),
                        LimitChoice::Continue => self.spent_by(&owner),
                        LimitChoice::ContinueUnlimited => f64::INFINITY,
                    };
                    lock(&self.limit_state.runs).insert(owner, next);
                }
            }
        }
        if let Some(max) = l.max_cost_per_session {
            let root = line[0].clone();
            let (total, floor) = (self.spent_by(&root), lock(&self.limit_state.session_floor).get(&root).copied().unwrap_or(0.0));
            if total - floor > max {
                let what = format!("This session has cost {} (limit {})", usd(total), usd(max));
                crate::log_warn!("engine", "limit reached session={sid}: {what}");
                let next = match self.ask_to_continue(ui, &what) {
                    LimitChoice::Stop => return Some(RunOutcome::LimitReached { what }),
                    LimitChoice::Continue => total,
                    LimitChoice::ContinueUnlimited => f64::INFINITY,
                };
                lock(&self.limit_state.session_floor).insert(root, next);
            }
        }
        None
    }

    /// A cost limit is set but this model has no price in the catalog, so its spend counts as zero. Said once per model.
    pub(super) fn note_unpriced_for_limits(&self, ui: &dyn Ui, model_ref: &str) {
        if self.cfg.limits.has_cost_limit() && lock(&self.unpriced_noted).insert(format!("limits:{model_ref}")) {
            ui.event(&UiEvent::Notice(&format!("{model_ref} has no price in the model catalog, so the cost limit cannot be enforced for it (the step limit still applies)")));
        }
    }
}
