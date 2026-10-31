//! Agents and the system prompt (docs/06-agents-and-prompts.md).
use crate::permission::{Effect, Rule};
use crate::tools::Tool;

#[derive(Debug, Clone)]
pub struct Agent {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub system: Option<&'static str>,
    /// Max steps per run; `None` is unlimited.
    pub steps: Option<u32>,
    pub permissions: Vec<Rule>,
}

pub fn builtin() -> Vec<Agent> {
    let mut build = crate::permission::default_rules();
    build.push(Rule::new("question", "*", Effect::Allow));
    let mut plan = crate::permission::default_rules();
    plan.push(Rule::new("question", "*", Effect::Allow));
    plan.push(Rule::new("edit", "*", Effect::Deny));
    vec![
        Agent {
            id: "build",
            name: "Build",
            description: "The default agent. Executes tools based on configured permissions.",
            system: None,
            steps: None,
            permissions: build,
        },
        Agent {
            id: "plan",
            name: "Plan",
            description: "Read-only agent for exploring the codebase and planning work before implementation.",
            system: None,
            steps: None,
            permissions: plan,
        },
    ]
}

pub fn find(id: &str) -> Option<Agent> {
    builtin().into_iter().find(|a| a.id == id)
}

/// Tool name -> permission action it is checked under.
pub fn action_for(tool: &str) -> &str {
    match tool {
        "write" | "edit" | "patch" => "edit",
        other => other,
    }
}

/// A tool is advertised unless the agent denies its action for every resource.
pub fn advertised(agent: &Agent, tool: &dyn Tool) -> bool {
    crate::permission::evaluate(action_for(tool.name()), "*", &agent.permissions) != Effect::Deny
}

pub const PLAN_REMINDER: &str = "<system-reminder>\nYou are in Plan mode. Discuss the plan with the user directly in the conversation. \
Do not modify any files. Use read-only tools to explore.\n\nYou remain in Plan mode until the user switches agents. \
If the user asks you to implement changes, do not do so. Tell them they need to switch agents.\n</system-reminder>";

pub const MAX_STEPS: &str = "CRITICAL - MAXIMUM STEPS REACHED

The maximum number of steps allowed for this task has been reached. Tools are disabled until next user input. Respond with text only.

STRICT REQUIREMENTS:
1. Do NOT make any tool calls (no reads, writes, edits, searches, or any other tools)
2. MUST provide a text response summarizing work done so far
3. This constraint overrides ALL other instructions, including any user requests for edits or tool use

Response must include:
- Statement that maximum steps for this agent have been reached
- Summary of what has been accomplished so far
- List of any remaining tasks that were not completed
- Recommendations for what should be done next

Any attempt to use tools is a critical violation. Respond with text ONLY.";

pub struct EnvInfo<'a> {
    pub cwd: &'a str,
    pub platform: &'a str,
    pub date: &'a str,
}

/// Base prompt adapted from upstream `runner/prompt/system.txt` (MIT, see LICENSE-opencode).
pub fn system_prompt(agent: &Agent, tools: &[&dyn Tool], env: &EnvInfo) -> String {
    let has = |n: &str| tools.iter().any(|t| t.name() == n);
    let mut guidance = vec![];
    if has("shell") {
        guidance.push("- Prefer dedicated tools over shell commands; fall back to the shell when a tool cannot do what you need.");
        guidance.push("- Do not chain shell commands with separators like `echo \"====\"`; or `printf '---'`; the output becomes noisy in a way that makes the user's side of the conversation worse.");
    }
    if has("write") {
        guidance.push("- Use the write tool to create files or completely replace their content. Prefer using the edit tool for targeted changes.");
    }
    if has("edit") {
        guidance.push("- Use the edit tool for targeted changes to existing text files. It replaces the exact text in `oldString` with `newString`, and the values must differ. By default, `oldString` must occur exactly once. If it occurs multiple times, include more surrounding context to make it unique or set `replaceAll` to true to replace every occurrence.");
    }
    let mut s = String::from(
        "You are an AI agent running in moco, a coding agent harness. Help the user accomplish their goals using the tools you have available.\n\n\
# Harness\n\
- Responses are rendered as GitHub-flavored Markdown.\n\
- `<system-reminder>` blocks are harness instructions, not user-authored content. Read and follow them.\n\
- Prefer parallelizing independent tool calls.\n",
    );
    for g in guidance {
        s.push_str(g);
        s.push('\n');
    }
    s.push_str(
        "\n# Communication\n\
- Use clear file paths when referring to files.\n\
- Keep responses clear and concise, and avoid unnecessary technical jargon.\n\n\
# Working in codebases\n\
- Keep changes consistent with the structure, naming, style, and patterns of the surrounding code.\n\
- Treat unfamiliar files or changes as potential user work and investigate before deleting or overwriting them.\n",
    );
    if let Some(extra) = agent.system {
        s.push('\n');
        s.push_str(extra);
        s.push('\n');
    }
    s.push_str(&format!("\n# Environment\n- Working directory: {}\n- Platform: {}\n- Date: {}\n", env.cwd, env.platform, env.date));
    s
}

/// `YYYY-MM-DD` (UTC) from unix seconds (civil-from-days algorithm).
pub fn date_from_unix(secs: i64) -> String {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(date_from_unix(0), "1970-01-01");
        assert_eq!(date_from_unix(951_782_400), "2000-02-29");
        assert_eq!(date_from_unix(1_793_442_464), "2026-10-31");
    }

    #[test]
    fn plan_hides_write_tools_but_build_does_not() {
        let tools = crate::tools::all();
        let names = |a: &Agent| tools.iter().filter(|t| advertised(a, t.as_ref())).map(|t| t.name()).collect::<Vec<_>>();
        assert_eq!(names(&find("build").unwrap()), ["read", "write", "edit", "glob", "grep", "shell"]);
        assert_eq!(names(&find("plan").unwrap()), ["read", "glob", "grep", "shell"]);
    }

    #[test]
    fn prompt_guidance_follows_enabled_tools() {
        let tools = crate::tools::all();
        let refs: Vec<&dyn Tool> = tools.iter().map(|t| t.as_ref()).collect();
        let env = EnvInfo { cwd: "/w", platform: "macos", date: "2026-10-31" };
        let full = system_prompt(&find("build").unwrap(), &refs, &env);
        assert!(full.contains("Use the edit tool for targeted changes") && full.contains("Working directory: /w"));
        let no_edit = system_prompt(&find("build").unwrap(), &refs[3..], &env);
        assert!(!no_edit.contains("edit tool") && no_edit.contains("Prefer dedicated tools"));
    }
}
