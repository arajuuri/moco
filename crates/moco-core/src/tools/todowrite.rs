use super::*;

pub struct TodoWrite;

const STATUSES: &[&str] = &["pending", "in_progress", "completed", "cancelled"];

impl Tool for TodoWrite {
    fn name(&self) -> &'static str {
        "todowrite"
    }
    fn description(&self) -> String {
        "Create and update a todo list for the current session. The user sees it as your working plan. Use it proactively when a task has 3 or more \
distinct steps. Send the full list on every call; it replaces the previous one. Mark items `in_progress` when you start them and `completed` as soon \
as they're actually done. Only call it when the list changes."
            .into()
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"todos":{"type":"array","items":{"type":"object","properties":{
            "content":{"type":"string","description":"Brief description of the task"},
            "status":{"type":"string","enum":STATUSES}},"required":["content","status"]}}},"required":["todos"]})
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let todos: Vec<Todo> = serde_json::from_value(input.get("todos").cloned().unwrap_or(Value::Null))
            .map_err(|e| ToolError::fail(format!("`todos` must be a list of {{content, status}}: {e}")))?;
        if let Some(bad) = todos.iter().find(|t| !STATUSES.contains(&t.status.as_str())) {
            return Err(ToolError::fail(format!("invalid status '{}'; use one of: {}", bad.status, STATUSES.join(", "))));
        }
        if todos.iter().filter(|t| t.status == "in_progress").count() > 1 {
            return Err(ToolError::fail("only one todo may be in_progress at a time"));
        }
        let open = todos.iter().filter(|t| t.status != "completed" && t.status != "cancelled").count();
        let n = todos.len();
        ctx.host.set_todos(todos)?;
        Ok(ToolOutput { text: format!("Todo list updated ({open} of {n} open)."), metadata: json!({"open": open, "total": n}) })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::Env;
    use super::*;

    #[test]
    fn replaces_the_list_and_validates() {
        let env = Env::new();
        let o = TodoWrite
            .run(&json!({"todos":[{"content":"a","status":"completed"},{"content":"b","status":"in_progress"},{"content":"c","status":"pending"}]}), &env.ctx())
            .unwrap();
        assert_eq!(o.text, "Todo list updated (2 of 3 open).");
        assert_eq!(env.host.todos.lock().unwrap().len(), 3);
        TodoWrite.run(&json!({"todos":[]}), &env.ctx()).unwrap();
        assert!(env.host.todos.lock().unwrap().is_empty());
        let bad = TodoWrite.run(&json!({"todos":[{"content":"a","status":"done"}]}), &env.ctx()).unwrap_err();
        assert!(bad.message().contains("invalid status 'done'"));
        let two = TodoWrite.run(&json!({"todos":[{"content":"a","status":"in_progress"},{"content":"b","status":"in_progress"}]}), &env.ctx()).unwrap_err();
        assert!(two.message().contains("only one"));
        assert!(TodoWrite.run(&json!({}), &env.ctx()).is_err());
    }
}
