//! Permission rule evaluation and user prompting for tool calls.
use super::*;

/// Rule evaluation + user prompting for tool permission requests.
pub(super) struct EnginePermitter<'a> {
    pub(super) rules: Vec<Rule>,
    pub(super) project_id: String,
    pub(super) store: &'a Mutex<Store>,
    pub(super) ui: &'a dyn Ui,
    /// Set when the user declines without feedback: every other pending/future request of the batch fails too.
    pub(super) declined: &'a AtomicBool,
    pub(super) ask_lock: &'a Mutex<()>,
    /// Present when the command classifier is on for this step.
    pub(super) classify: Option<classify::ClassifyCtx<'a>>,
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
            crate::log_debug!("classifier", "session={} skipped (opaque or never-classify list): {}", c.session.id, classifier::sanitize(command, 200));
            return false;
        }
        let saved = self.store.lock().unwrap_or_else(|e| e.into_inner()).saved_permissions(&self.project_id).unwrap_or_default();
        if !permission::only_default_asks(&req.action, &req.resources, &self.rules, &saved) {
            crate::log_debug!("classifier", "session={} skipped (an explicit rule decides): {}", c.session.id, classifier::sanitize(command, 200));
            return false;
        }
        match c.engine.classify_command(c.session, command, &parsed, c.settings, self.ui, c.cancel) {
            classifier::Verdict::Allow { risk, reason } => {
                // Its own event, not a Notice: an allowed command is not an error, and the TUI marks the tool row with it.
                self.ui.event(&UiEvent::AutoAllowed { call_id: &req.call_id, command: &classifier::sanitize(command, 100), risk: &risk, reason: &reason });
                true
            }
            classifier::Verdict::Ask { reason } => {
                if !reason.is_empty() {
                    self.ui.event(&UiEvent::Status(&format!("classifier: asking you because {reason}")));
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
                            st.save_permission(&self.project_id, &req.action, pat).log_err("store", "saving an always-allow rule");
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
