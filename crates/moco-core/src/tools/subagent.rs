use super::*;

pub struct Subagent {
    /// `(id, description)` of the agents that may be started.
    pub agents: Vec<(String, String)>,
}

impl Tool for Subagent {
    fn name(&self) -> &'static str {
        "subagent"
    }
    fn description(&self) -> String {
        let list = self.agents.iter().map(|(id, d)| format!("- {id}: {d}")).collect::<Vec<_>>().join("\n");
        format!(
            "Launch a subagent to handle a self-contained task in a fresh context. Its final message is returned to you; the user does not see it. \
The `prompt` is the subagent's only input, so include everything it needs. Run independent subagents in parallel by calling this tool several times in one \
response. Pass a previous `sessionID` to continue that subagent's conversation. With `background: true` the call returns at once and \
you are told when the subagent finishes (do not poll); a background subagent cannot ask the user for permission, so give it work that needs none.\n\nAvailable agents:\n{list}"
        )
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "agent":{"type":"string","description":"Which agent to run"},
            "description":{"type":"string","description":"A short (3-5 words) description of the task"},
            "prompt":{"type":"string","description":"The task for the subagent to perform"},
            "model":{"type":"string","description":"Optional provider/model override"},
            "sessionID":{"type":"string","description":"Continue an earlier subagent session"},
            "background":{"type":"boolean","description":"Return at once; you are told when it finishes"}},
            "required":["agent","description","prompt"]})
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let agent = str_arg(input, "agent")?;
        if !self.agents.iter().any(|(id, _)| id == agent) {
            let ids: Vec<&str> = self.agents.iter().map(|(id, _)| id.as_str()).collect();
            return Err(ToolError::fail(format!("Unknown agent '{agent}'. Available: {}", ids.join(", "))));
        }
        let req = SubagentRequest {
            agent: agent.to_string(),
            description: str_arg(input, "description")?.to_string(),
            prompt: str_arg(input, "prompt")?.to_string(),
            model: opt_str(input, "model").map(String::from),
            session_id: opt_str(input, "sessionID").map(String::from),
        };
        ctx.permit("subagent", vec![agent.to_string()], vec!["*".into()])?;
        if opt_bool(input, "background").unwrap_or(false) {
            let id = ctx.host.start_background_subagent(req)?;
            return Ok(ToolOutput {
                text: format!("Subagent started in the background (sessionID: {id}). You will be told when it finishes; do not poll for it."),
                metadata: json!({"sessionID": id, "status": "running"}),
            });
        }
        let r = ctx.host.subagent(req)?;
        Ok(ToolOutput {
            text: format!("sessionID: {}\n\n{}", r.session_id, r.output),
            metadata: json!({"sessionID": r.session_id, "status": "completed"}),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::Env;
    use super::*;

    fn tool() -> Subagent {
        Subagent { agents: vec![("general".into(), "does things".into()), ("explore".into(), "searches".into())] }
    }

    #[test]
    fn description_lists_agents_and_run_delegates_to_the_host() {
        let env = Env::new();
        let t = tool();
        assert!(t.description().contains("- general: does things\n- explore: searches"));
        let o = t.run(&json!({"agent":"explore","description":"find it","prompt":"look for X","sessionID":"ses_old","model":"openai/gpt-x"}), &env.ctx()).unwrap();
        assert_eq!(o.text, "sessionID: ses_child\n\ndone: look for X");
        let req = env.host.subagents.lock().unwrap()[0].clone();
        assert_eq!((req.agent.as_str(), req.session_id.as_deref(), req.model.as_deref()), ("explore", Some("ses_old"), Some("openai/gpt-x")));
        assert_eq!(env.perm.asked()[0].action, "subagent");
    }

    #[test]
    fn rejects_unknown_agents() {
        let env = Env::new();
        let t = tool();
        let e = t.run(&json!({"agent":"nope","description":"d","prompt":"p"}), &env.ctx()).unwrap_err();
        assert_eq!(e.message(), "Unknown agent 'nope'. Available: general, explore");
        assert!(env.host.subagents.lock().unwrap().is_empty());
    }

    #[test]
    fn background_returns_at_once_with_the_session_id() {
        let env = Env::new();
        let o = tool().run(&json!({"agent":"general","description":"d","prompt":"long job","background":true}), &env.ctx()).unwrap();
        assert!(o.text.starts_with("Subagent started in the background (sessionID: ses_bg)") && o.text.contains("do not poll"));
        assert_eq!(o.metadata["status"], "running");
        assert_eq!(env.host.subagents.lock().unwrap()[0].prompt, "long job");
        assert_eq!(env.perm.asked()[0].action, "subagent", "permission is still checked");
    }
}
