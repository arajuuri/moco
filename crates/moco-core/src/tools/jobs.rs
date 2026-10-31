use super::*;

pub struct Jobs;

impl Tool for Jobs {
    fn name(&self) -> &str {
        "jobs"
    }
    fn description(&self) -> String {
        "Inspect or stop background jobs (shell commands started with `background: true` and background subagents). \
`list` shows every job and its state; `output` shows a job's output so far (`lines`, default 60); `kill` stops a running job. \
You are told automatically when a job finishes, so use this only to look at output while it runs or to stop it."
            .into()
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["list","output","kill"]},
            "id":{"type":"string","description":"Job id (shell ID or subagent session ID)"},
            "lines":{"type":"integer","minimum":1,"description":"For output: how many trailing lines (default 60)"}},"required":["action"]})
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let action = str_arg(input, "action")?;
        let id = || str_arg(input, "id").map_err(|_| ToolError::fail(format!("`id` is required for {action}")));
        match action {
            "list" => {
                let jobs = ctx.host.jobs();
                if jobs.is_empty() {
                    return Ok(ToolOutput::text("No background jobs."));
                }
                let lines: Vec<String> = jobs.iter().map(|j| format!("{}  {}  {}  {}s  {}", j.id, j.kind, j.status, j.seconds, j.title.lines().next().unwrap_or(""))).collect();
                Ok(ToolOutput { text: lines.join("\n"), metadata: json!({"count": jobs.len()}) })
            }
            "output" => Ok(ToolOutput::text(ctx.host.job_output(id()?, opt_u64(input, "lines").unwrap_or(60) as usize)?)),
            "kill" => {
                let id = id()?;
                ctx.permit("jobs", vec![format!("kill {id}")], vec!["*".into()])?;
                Ok(ToolOutput::text(ctx.host.kill_job(id)?))
            }
            other => Err(ToolError::fail(format!("unknown action '{other}'; use list, output or kill"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::Env;
    use super::*;

    #[test]
    fn list_output_and_kill() {
        let env = Env::new();
        assert_eq!(Jobs.run(&json!({"action":"list"}), &env.ctx()).unwrap().text, "No background jobs.");
        env.host.jobs.lock().unwrap().push(JobInfo { id: "shl_1".into(), kind: "shell".into(), title: "npm run dev\nsecond line".into(), status: "running".into(), running: true, seconds: 12, output_path: None });
        assert_eq!(Jobs.run(&json!({"action":"list"}), &env.ctx()).unwrap().text, "shl_1  shell  running  12s  npm run dev");
        assert_eq!(Jobs.run(&json!({"action":"output","id":"shl_1","lines":5}), &env.ctx()).unwrap().text, "output of shl_1 (5 lines)");
        assert_eq!(Jobs.run(&json!({"action":"kill","id":"shl_1"}), &env.ctx()).unwrap().text, "stopped shl_1");
        let asked = env.perm.asked();
        assert_eq!((asked.len(), asked[0].action.as_str(), asked[0].resources.clone()), (1, "jobs", vec!["kill shl_1".to_string()]), "only kill asks");
        assert!(Jobs.run(&json!({"action":"kill"}), &env.ctx()).unwrap_err().message().contains("`id` is required"));
        assert!(Jobs.run(&json!({"action":"restart"}), &env.ctx()).unwrap_err().message().contains("unknown action"));
        *env.perm.reply.lock().unwrap() = Err(ToolError::Declined);
        assert_eq!(Jobs.run(&json!({"action":"kill","id":"x"}), &env.ctx()).unwrap_err(), ToolError::Declined);
    }
}
