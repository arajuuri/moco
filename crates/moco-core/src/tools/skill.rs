use super::*;
use crate::skills::{self, Skill};

pub struct SkillTool {
    pub skills: Vec<Skill>,
}

impl Tool for SkillTool {
    fn name(&self) -> &'static str {
        "skill"
    }
    fn description(&self) -> String {
        "Load a skill: specialized instructions for a kind of task, with any scripts or reference files it ships. \
The available skills and when to use them are listed in the system prompt. Load the matching skill before starting the task, then follow it."
            .into()
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"id":{"type":"string","description":"Skill id from the available_skills list"}},"required":["id"]})
    }
    fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let id = str_arg(input, "id")?;
        let Some(skill) = self.skills.iter().find(|s| s.id == id) else {
            let ids: Vec<&str> = self.skills.iter().map(|s| s.id.as_str()).collect();
            return Err(ToolError::fail(format!("Unknown skill '{id}'. Available: {}", ids.join(", "))));
        };
        ctx.permit("skill", vec![id.to_string()], vec!["*".into()])?;
        Ok(ToolOutput { text: skills::render_content(skill), metadata: json!({"id": skill.id, "dir": skill.dir.to_string_lossy()}) })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::Env;
    use super::*;

    #[test]
    fn loads_a_skill_by_id_and_rejects_unknown_ones() {
        let env = Env::new();
        let dir = env.dir.join("sk/pdf");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("helper.py"), "x").unwrap();
        let t = SkillTool { skills: vec![Skill { id: "pdf".into(), name: "PDF tools".into(), description: "d".into(), dir, body: "Do the thing.".into() }] };
        let o = t.run(&json!({"id":"pdf"}), &env.ctx()).unwrap();
        assert!(o.text.contains("# Skill: PDF tools") && o.text.contains("Do the thing.") && o.text.contains("<file>helper.py</file>"), "{}", o.text);
        assert_eq!((env.perm.asked()[0].action.as_str(), env.perm.asked()[0].resources.clone()), ("skill", vec!["pdf".to_string()]));
        assert_eq!(t.run(&json!({"id":"nope"}), &env.ctx()).unwrap_err().message(), "Unknown skill 'nope'. Available: pdf");
    }
}
