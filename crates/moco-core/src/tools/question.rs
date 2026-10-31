use super::*;

pub struct AskQuestion;

impl Tool for AskQuestion {
    fn name(&self) -> &'static str {
        "question"
    }
    fn description(&self) -> String {
        "Ask the user one or more questions when you need a decision or missing information that you cannot find yourself. Each question has a short \
header and a list of options (label of 1-5 words plus a description); the user can also type their own answer. Set `multiple` to let them pick several \
options. Do not use it for things you can look up or decide reasonably."
            .into()
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"questions":{"type":"array","minItems":1,"items":{"type":"object","properties":{
            "question":{"type":"string"},
            "header":{"type":"string","maxLength":30,"description":"Very short label"},
            "options":{"type":"array","items":{"type":"object","properties":{
                "label":{"type":"string","description":"1-5 words"},"description":{"type":"string"}},"required":["label","description"]}},
            "multiple":{"type":"boolean"}},"required":["question","header","options"]}}},"required":["questions"]})
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let questions: Vec<Question> = serde_json::from_value(input.get("questions").cloned().unwrap_or(Value::Null))
            .map_err(|e| ToolError::fail(format!("`questions` must be a non-empty list of {{question, header, options}}: {e}")))?;
        if questions.is_empty() {
            return Err(ToolError::fail("`questions` must not be empty"));
        }
        ctx.permit("question", questions.iter().map(|q| q.question.clone()).collect(), vec!["*".into()])?;
        let answers = ctx.host.ask(&questions)?;
        let text = questions
            .iter()
            .zip(&answers)
            .map(|(q, a)| format!("\"{}\"=\"{}\"", q.question, if a.is_empty() { "(no answer)".to_string() } else { a.join(", ") }))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput { text: format!("User answered:\n{text}"), metadata: json!({"answers": answers}) })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::Env;
    use super::*;

    fn q() -> Value {
        json!({"questions":[
            {"question":"Which DB?","header":"Database","options":[{"label":"SQLite","description":"embedded"},{"label":"Postgres","description":"server"}]},
            {"question":"Extras?","header":"Extras","multiple":true,"options":[]}]})
    }

    #[test]
    fn answers_come_back_per_question() {
        let env = Env::new();
        *env.host.answers.lock().unwrap() = Some(vec![vec!["SQLite".into()], vec!["tests".into(), "docs".into()]]);
        let o = AskQuestion.run(&q(), &env.ctx()).unwrap();
        assert_eq!(o.text, "User answered:\n\"Which DB?\"=\"SQLite\"\n\"Extras?\"=\"tests, docs\"");
        assert_eq!(o.metadata["answers"][1], json!(["tests", "docs"]));
        let asked = env.host.asked.lock().unwrap();
        assert_eq!((asked[0][0].header.as_str(), asked[0][0].options.len(), asked[0][1].multiple), ("Database", 2, true));
    }

    #[test]
    fn dismissal_stops_and_bad_input_is_a_model_error() {
        let env = Env::new();
        assert_eq!(AskQuestion.run(&q(), &env.ctx()).unwrap_err(), ToolError::Declined, "dismissed form aborts the step");
        assert!(AskQuestion.run(&json!({"questions":[]}), &env.ctx()).unwrap_err().message().contains("must not be empty"));
        assert!(AskQuestion.run(&json!({"questions":[{"question":"x"}]}), &env.ctx()).is_err());
    }
}
