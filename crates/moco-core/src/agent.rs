//! Agents and the system prompt (docs/06-agents-and-prompts.md).
use crate::permission::{Effect, Rule};
use crate::tools::Tool;

#[derive(Debug, Clone)]
pub struct Agent {
    pub id: String,
    pub mode: Mode,
    pub name: String,
    pub description: String,
    pub system: Option<String>,
    /// `provider/model` override for this agent.
    pub model: Option<String>,
    /// Max steps per run; `None` is unlimited.
    pub steps: Option<u32>,
    pub permissions: Vec<Rule>,
}

/// Where an agent may be used: chosen by the user (`Primary`), started by the subagent tool (`Subagent`), or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Primary,
    Subagent,
    All,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "primary" => Some(Mode::Primary),
            "subagent" => Some(Mode::Subagent),
            "all" => Some(Mode::All),
            _ => None,
        }
    }
}

impl Agent {
    /// The user can pick it as the session agent.
    pub fn selectable(&self) -> bool {
        self.mode != Mode::Subagent
    }
    /// The subagent tool may start it.
    pub fn subagent(&self) -> bool {
        self.mode != Mode::Primary
    }
}

pub const EXPLORE_PROMPT: &str = "You are a file search specialist. You excel at thoroughly navigating and exploring codebases.

Guidelines:
- Your role is EXCLUSIVELY to search and analyze
- Parallelize independent tool calls for searches and reads whenever possible
- Adapt your search approach based on the thoroughness level specified by the caller
- Return file paths as absolute paths in your final response
- You MUST NOT create, modify, delete, move, or copy files, including temporary files and reports
- Shell commands MUST be read-only. NEVER run commands that write files or change system state

Complete the user's search request efficiently and report your findings clearly.";

pub fn builtin() -> Vec<Agent> {
    let mut build = crate::permission::default_rules();
    build.push(Rule::new("question", "*", Effect::Allow));
    let mut plan = crate::permission::default_rules();
    plan.push(Rule::new("question", "*", Effect::Allow));
    plan.push(Rule::new("edit", "*", Effect::Deny));
    let mut general = crate::permission::default_rules();
    general.push(Rule::new("question", "*", Effect::Deny));
    general.push(Rule::new("subagent", "*", Effect::Deny));
    let mut explore = crate::permission::default_rules();
    explore.extend([
        Rule::new("*", "*", Effect::Deny),
        Rule::new("shell", "*", Effect::Allow),
        Rule::new("grep", "*", Effect::Allow),
        Rule::new("glob", "*", Effect::Allow),
        Rule::new("webfetch", "*", Effect::Allow),
        Rule::new("websearch", "*", Effect::Ask),
        Rule::new("read", "*", Effect::Allow),
        Rule::new("read", "*.env", Effect::Ask),
        Rule::new("read", "*.env.*", Effect::Ask),
        Rule::new("read", "*.env.example", Effect::Allow),
        Rule::new("subagent", "*", Effect::Deny),
        Rule::new("external_directory", "*", Effect::Allow),
    ]);
    vec![
        Agent {
            id: "build".into(),
            mode: Mode::Primary,
            name: "Build".into(),
            description: "The default agent. Executes tools based on configured permissions.".into(),
            system: None,
            model: None,
            steps: None,
            permissions: build,
        },
        Agent {
            id: "plan".into(),
            mode: Mode::Primary,
            name: "Plan".into(),
            description: "Read-only agent for exploring the codebase and planning work before implementation.".into(),
            system: None,
            model: None,
            steps: None,
            permissions: plan,
        },
        Agent {
            id: "general".into(),
            mode: Mode::Subagent,
            name: "General".into(),
            description: "General-purpose agent for researching complex questions and executing multi-step tasks. Use this agent to execute multiple units of work in parallel.".into(),
            system: None,
            model: None,
            steps: None,
            permissions: general,
        },
        Agent {
            id: "explore".into(),
            mode: Mode::Subagent,
            name: "Explore".into(),
            description: "Fast agent specialized for exploring codebases: finding files by pattern, searching code for keywords, answering questions about the code. Read-only. Say how thorough the search should be: \"quick\", \"medium\" or \"very thorough\".".into(),
            system: Some(EXPLORE_PROMPT.into()),
            model: None,
            steps: None,
            permissions: explore,
        },
    ]
}

pub fn find(agents: &[Agent], id: &str) -> Option<Agent> {
    agents.iter().find(|a| a.id == id).cloned()
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
    /// Rendered `AGENTS.md` content (see `instructions::render`), may be empty.
    pub instructions: &'a str,
    /// Rendered skill list (`skills::render_list`); only included when the agent has the `skill` tool.
    pub skills: &'a str,
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
    if let Some(extra) = &agent.system {
        s.push('\n');
        s.push_str(extra);
        s.push('\n');
    }
    if !env.instructions.trim().is_empty() {
        s.push_str("\n# Project instructions\nThese come from the user's AGENTS.md files; follow them. Later entries are more specific and win over earlier ones.\n\n");
        s.push_str(env.instructions);
    }
    if has("skill") && !env.skills.trim().is_empty() {
        s.push_str("\n# Skills\n");
        s.push_str(env.skills);
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
    fn modes_and_explore_permissions() {
        let agents = builtin();
        let sel: Vec<_> = agents.iter().filter(|a| a.selectable()).map(|a| a.id.as_str()).collect();
        let sub: Vec<_> = agents.iter().filter(|a| a.subagent()).map(|a| a.id.as_str()).collect();
        assert_eq!((sel, sub), (vec!["build", "plan"], vec!["general", "explore"]));
        let ex = find(&agents, "explore").unwrap();
        use crate::permission::evaluate;
        assert_eq!(evaluate("write", "x", &ex.permissions), Effect::Deny);
        assert_eq!(evaluate("edit", "x", &ex.permissions), Effect::Deny);
        assert_eq!(evaluate("shell", "ls", &ex.permissions), Effect::Allow);
        assert_eq!(evaluate("read", ".env", &ex.permissions), Effect::Ask);
        assert_eq!(evaluate("read", ".env.example", &ex.permissions), Effect::Allow);
        assert_eq!(evaluate("subagent", "general", &ex.permissions), Effect::Deny);
        assert_eq!(evaluate("external_directory", "/etc/*", &ex.permissions), Effect::Allow);
        assert!(ex.system.as_deref().unwrap().starts_with("You are a file search specialist"));
    }

    #[test]
    fn dates() {
        assert_eq!(date_from_unix(0), "1970-01-01");
        assert_eq!(date_from_unix(951_782_400), "2000-02-29");
        assert_eq!(date_from_unix(1_793_442_464), "2026-10-31");
    }

    #[test]
    fn plan_hides_write_tools_but_build_does_not() {
        let tools = crate::tools::all(vec![], vec![], None);
        let names = |a: &Agent| tools.iter().filter(|t| advertised(a, t.as_ref())).map(|t| t.name()).collect::<Vec<_>>();
        let all = ["read", "write", "edit", "patch", "glob", "grep", "shell", "jobs", "webfetch", "todowrite", "question", "subagent"];
        assert_eq!(names(&find(&builtin(), "build").unwrap()), all);
        assert_eq!(names(&find(&builtin(), "plan").unwrap()), ["read", "glob", "grep", "shell", "jobs", "webfetch", "todowrite", "question", "subagent"]);
        assert_eq!(names(&find(&builtin(), "general").unwrap()), ["read", "write", "edit", "patch", "glob", "grep", "shell", "jobs", "webfetch", "todowrite"], "no question, no subagent");
        assert_eq!(names(&find(&builtin(), "explore").unwrap()), ["read", "glob", "grep", "shell", "webfetch"], "read-only");
    }

    #[test]
    fn websearch_is_offered_to_every_agent_but_always_asks() {
        let tools = crate::tools::all(vec![], vec![], Some(crate::tools::websearch::Settings::default_for("/x".into())));
        let names = |a: &Agent| tools.iter().filter(|t| advertised(a, t.as_ref())).map(|t| t.name()).collect::<Vec<_>>();
        for id in ["build", "plan", "general", "explore"] {
            let n = names(&find(&builtin(), id).unwrap());
            let at = n.iter().position(|x| *x == "websearch").unwrap_or_else(|| panic!("{id}: {n:?}"));
            assert_eq!(n[at - 1], "webfetch", "{id}: it sits right after webfetch");
            assert_eq!(crate::permission::evaluate("websearch", "some query", &find(&builtin(), id).unwrap().permissions), Effect::Ask, "{id}");
        }
    }

    #[test]
    fn prompt_guidance_follows_enabled_tools() {
        let tools = crate::tools::all(vec![], vec![], None);
        let refs: Vec<&dyn Tool> = tools.iter().map(|t| t.as_ref()).collect();
        let env = EnvInfo { cwd: "/w", platform: "macos", date: "2026-10-31", instructions: "", skills: "" };
        let full = system_prompt(&find(&builtin(), "build").unwrap(), &refs, &env);
        assert!(full.contains("Use the edit tool for targeted changes") && full.contains("Working directory: /w"));
        let no_edit = system_prompt(&find(&builtin(), "build").unwrap(), &refs[4..], &env);
        assert!(!no_edit.contains("edit tool") && no_edit.contains("Prefer dedicated tools"));
        assert!(!full.contains("Project instructions"));
        let with = EnvInfo { instructions: "Instructions from: /w/AGENTS.md\nuse tabs\n", ..env };
        let p = system_prompt(&find(&builtin(), "build").unwrap(), &refs, &with);
        assert!(p.contains("# Project instructions") && p.contains("use tabs"));
    }
}
