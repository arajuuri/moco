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
    /// The engine's record of store failures already shown (see `notice_once`).
    pub(super) notified: &'a Mutex<HashSet<String>>,
    /// Present when the command classifier is on for this step.
    pub(super) classify: Option<classify::ClassifyCtx<'a>>,
    /// The saved rules as last read, with the `store::SAVED_PERMISSIONS_GEN` they were read at (see `saved`).
    pub(super) cache: Mutex<Option<(u64, Vec<Rule>)>>,
}

impl EnginePermitter<'_> {
    /// The rules saved with "always". When they cannot be read the user is told once, because every request then
    /// asks again as if nothing had been saved.
    fn saved(&self) -> Vec<Rule> {
        // Read once per step instead of from SQLite (under the shared store lock) on every check. Only a successful
        // read is cached, so a failure keeps being retried and reported as before. A save or removal in this process
        // (an "always" answer, `/permissions remove`) bumps the generation and so drops the copy; a `moco permissions`
        // run from another process takes effect at the next step.
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let gen = crate::store::SAVED_PERMISSIONS_GEN.load(Ordering::Relaxed);
        if let Some((at, rules)) = cache.as_ref() {
            if *at == gen {
                return rules.clone();
            }
        }
        let r = self.store.lock().unwrap_or_else(|e| e.into_inner()).saved_permissions(&self.project_id);
        let ok = r.is_ok();
        let rules = notice_once(self.notified, self.ui, "saved-permissions-read", "saved permissions could not be read, so you may be asked again for things you answered \"always\" to", r).unwrap_or_default();
        if ok {
            *cache = Some((gen, rules.clone()));
        }
        rules
    }

    /// The classifier may answer for a shell command that would only ask because of a default rule. It never
    /// overrides a rule the user or the careful preset's always-ask list wrote, and never sees opaque or dangerous
    /// commands. `true` = run it without asking.
    fn auto_allowed(&self, req: &PermReq) -> bool {
        let (Some(c), Some(command)) = (&self.classify, &req.command) else { return false };
        if req.action != "shell" || req.force_ask {
            return false;
        }
        let parsed = tools::shellparse::parse(command);
        if classifier::skip_classifier(command, &parsed) {
            crate::log_debug!("classifier", "session={} skipped (opaque or never-classify list): {}", c.session.id, classifier::sanitize(command, 200));
            return false;
        }
        let saved = self.saved();
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
        let saved = self.saved();
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
                        for pat in &req.save {
                            let r = self.store.lock().unwrap_or_else(|e| e.into_inner()).save_permission(&self.project_id, &req.action, pat);
                            notice_once(self.notified, self.ui, "saved-permissions-write", "saved permissions could not be stored, so this \"always\" answer is not remembered and you will be asked again", r);
                        }
                        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what it was asked and refuses, so a request that is no longer allowed shows up as a prompt.
    #[derive(Default)]
    struct Refuse(Mutex<Vec<String>>);
    impl Ui for Refuse {
        fn event(&self, _: &UiEvent) {}
        fn ask(&self, p: &PermissionPrompt) -> Reply {
            self.0.lock().unwrap().push(p.resources.join(" "));
            Reply::Reject(Some("no".into()))
        }
    }

    #[test]
    fn a_saved_rule_removed_during_a_step_stops_allowing_at_once() {
        let dir = std::env::temp_dir().join(crate::id::new_id("moco-permit"));
        let store = Store::open(&dir.join("moco.db")).unwrap();
        let project_id = store.project_for(&dir.to_string_lossy()).unwrap();
        store.save_permission(&project_id, "shell", "osascript *").unwrap();
        let store = Mutex::new(store);
        let (ui, declined, ask_lock, notified) = (Refuse::default(), AtomicBool::new(false), Mutex::new(()), Mutex::new(HashSet::new()));
        let p = EnginePermitter { rules: vec![], project_id: project_id.clone(), store: &store, ui: &ui, declined: &declined, ask_lock: &ask_lock, notified: &notified, classify: None, cache: Mutex::new(None) };
        let req = PermReq { action: "shell".into(), resources: vec!["osascript -e 'beep'".into()], save: vec!["osascript *".into()], force_ask: false, command: None, call_id: "t1".into() };

        assert!(p.check(&req).is_ok(), "the saved rule allows it");
        assert!(ui.0.lock().unwrap().is_empty());
        // `/permissions remove` from the front end, while the step (and its cached copy of the rules) is still alive.
        crate::saved_permissions::manage(&store.lock().unwrap(), &project_id, "/permissions", "remove 1").unwrap();
        assert!(matches!(p.check(&req), Err(ToolError::Corrected(_))), "asked again, and the test UI refused");
        assert_eq!(ui.0.lock().unwrap().as_slice(), ["osascript -e 'beep'"]);
    }
}
