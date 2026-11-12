//! Custom slash commands (docs/08 "Custom commands"): prompt templates in `commands/` (or `command/`) directories of the
//! config roots, in config `commands`, and a few built in. `$ARGUMENTS` is replaced by what follows the command; a
//! template without it gets the arguments appended after a blank line.
use crate::config::CommandConfig;
use crate::markdown;
use crate::skills::config_roots;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub template: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    /// Run in a child session.
    pub subagent: bool,
    /// `built-in`, `config` or the file it came from.
    pub source: String,
}

const INIT: &str = "Create or update AGENTS.md in the root of this project: the guide a coding agent needs to be productive here.

First explore: the README, build and package manifests, test and CI configuration, the directory layout, and a few representative source files. If AGENTS.md (or CLAUDE.md) already exists, read it and improve it instead of starting over.

Write down only what is not obvious from a quick look at the code:
- the commands to build, lint and run the tests, including how to run a single test
- the architecture that needs several files to understand
- conventions and pitfalls specific to this project, and anything it relies on that is easy to miss (required tools, environment variables, generated files)

Leave out generic advice, lists of every file, and anything you could not verify. Keep it short.

$ARGUMENTS";

const REVIEW: &str = "Review the current changes in this repository and report problems, ordered by severity.

Start with `git status` and `git diff` (staged and unstaged), and look at untracked files that belong to the change. If the arguments below name a commit, branch or pull request, review that instead. Read the surrounding code where needed; do not judge a hunk in isolation.

Look for: bugs and wrong edge-case behavior, missing error handling, security problems, broken or missing tests, and places where the change does not follow the surrounding code's conventions. For each finding give the file and line, what is wrong, and a concrete fix. If nothing is wrong, say so plainly. Do not modify any files.

$ARGUMENTS";

pub fn builtin() -> Vec<Command> {
    let mk = |name: &str, description: &str, template: &str| Command {
        name: name.into(),
        description: description.into(),
        template: template.into(),
        agent: None,
        model: None,
        subagent: false,
        source: "built-in".into(),
    };
    vec![
        mk("init", "Create or update AGENTS.md for this project", INIT),
        mk("review", "Review the working-tree changes (or a commit, branch or PR given as argument)", REVIEW),
    ]
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
}

fn from_file(path: &Path) -> Option<Command> {
    let name = path.file_stem()?.to_str()?.to_string();
    if !valid_name(&name) || path.extension()? != "md" {
        return None;
    }
    let doc = markdown::parse(&std::fs::read_to_string(path).ok()?);
    let description = doc
        .get("description")
        .map(String::from)
        .or_else(|| doc.body.lines().map(str::trim).find(|l| !l.is_empty()).map(|l| l.chars().take(80).collect()))
        .unwrap_or_default();
    Some(Command {
        name,
        description,
        template: doc.body.trim_end().to_string(),
        agent: doc.get("agent").map(String::from),
        model: doc.get("model").map(String::from),
        subagent: doc.flag("subagent"),
        source: path.display().to_string(),
    })
}

/// Commands sorted by name. Later sources replace earlier ones with the same name: built-ins, then files in the
/// config roots (lowest priority root first), then `commands` from config.
pub fn discover(app_dir: &Path, cwd: &Path, configured: &BTreeMap<String, CommandConfig>) -> Vec<Command> {
    let mut found: BTreeMap<String, Command> = builtin().into_iter().map(|c| (c.name.clone(), c)).collect();
    for root in config_roots(app_dir, cwd) {
        for sub in ["command", "commands"] {
            let Ok(rd) = std::fs::read_dir(root.join(sub)) else { continue };
            let mut files: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_file()).collect();
            files.sort();
            for f in files {
                if let Some(c) = from_file(&f) {
                    found.insert(c.name.clone(), c);
                }
            }
        }
    }
    for (name, cc) in configured {
        if valid_name(name) && !cc.template.trim().is_empty() {
            found.insert(
                name.clone(),
                Command {
                    name: name.clone(),
                    description: cc.description.clone().unwrap_or_default(),
                    template: cc.template.clone(),
                    agent: cc.agent.clone(),
                    model: cc.model.clone(),
                    subagent: cc.subagent,
                    source: "config".into(),
                },
            );
        }
    }
    found.into_values().collect()
}

/// Warnings for built-in commands (`/review`, `/init`) that a project's command file or config replaced (#168): the
/// replacement runs whenever the user types the familiar name, so it should not go unnoticed. The user's own
/// replacements are not mentioned.
pub fn project_overrides(found: &[Command], app_dir: &Path, origins: &BTreeMap<String, std::path::PathBuf>) -> Vec<String> {
    let builtin: Vec<String> = builtin().into_iter().map(|c| c.name).collect();
    found
        .iter()
        .filter(|c| builtin.contains(&c.name) && !c.user_written(app_dir, origins))
        .map(|c| {
            let from = if c.source == "config" { origins.get(&c.name).map(|p| p.display().to_string()).unwrap_or_else(|| "a project config".into()) } else { c.source.clone() };
            format!("{from}: replaces the built-in command /{}", c.name)
        })
        .collect()
}

impl Command {
    /// Whether the user wrote this command: a built-in, a file in their own config roots (`~/.claude`, `~/.agents`,
    /// `<app dir>/config`), or a `commands` entry of their own config (`origins` is `config::Loaded::command_origins`).
    /// A project's command files and config entries are not, so their `@` mentions are checked (`mentions::Origin`).
    pub fn user_written(&self, app_dir: &Path, origins: &BTreeMap<String, std::path::PathBuf>) -> bool {
        match self.source.as_str() {
            "built-in" => true,
            "config" => origins.get(&self.name).is_some_and(|p| crate::config::is_user_level(app_dir, p)),
            // Directly in `<root>/commands/`: a repository checked out somewhere under `~/.claude` is not the user's.
            file => {
                let root = Path::new(file).parent().and_then(Path::parent);
                root.is_some_and(|root| crate::skills::user_roots(app_dir).iter().any(|r| r == root))
            }
        }
    }

    /// The `mentions::Origin` of this command's expanded text, `args` being what the user typed after its name.
    pub fn origin(&self, app_dir: &Path, origins: &BTreeMap<String, std::path::PathBuf>, args: &str) -> crate::mentions::Origin {
        if self.user_written(app_dir, origins) {
            crate::mentions::Origin::User
        } else {
            crate::mentions::Origin::Command { args: args.to_string() }
        }
    }
}

pub fn expand(c: &Command, args: &str) -> String {
    let args = args.trim();
    if c.template.contains("$ARGUMENTS") {
        c.template.replace("$ARGUMENTS", args).trim().to_string()
    } else if args.is_empty() {
        c.template.clone()
    } else {
        format!("{}\n\n{args}", c.template)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(crate::id::new_id("moco-cmds"));
        std::fs::create_dir_all(root.join("app/config")).unwrap();
        std::fs::create_dir_all(root.join("proj")).unwrap();
        root
    }
    fn put(p: std::path::PathBuf, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn builtins_files_and_config_layer_by_priority() {
        let r = tree();
        put(r.join("app/config/commands/deploy.md"), "---\ndescription: Deploy it\n---\nglobal deploy $ARGUMENTS");
        put(r.join("proj/.claude/commands/deploy.md"), "claude deploy");
        put(r.join("proj/.moco/command/test.md"), "---\ndescription: Run tests: all of them\nagent: plan\nmodel: openai/gpt-x\nsubagent: true\n---\nRun the tests.\n");
        put(r.join("proj/.moco/commands/first-line.md"), "\n  The first non-empty line becomes the description.\nrest");
        put(r.join("proj/.moco/commands/bad name.md"), "x");
        put(r.join("proj/.moco/commands/notes.txt"), "x");
        let cfg = BTreeMap::from([
            ("review".to_string(), CommandConfig { template: "my review of $ARGUMENTS".into(), description: Some("custom".into()), ..Default::default() }),
            ("empty".to_string(), CommandConfig::default()),
        ]);
        let got = discover(&r.join("app"), &r.join("proj"), &cfg);
        let names: Vec<_> = got.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["deploy", "first-line", "init", "review", "test"]);
        let by = |n: &str| got.iter().find(|c| c.name == n).unwrap();
        assert_eq!(by("deploy").template, "claude deploy", ".claude beats the global config root");
        assert_eq!((by("review").description.as_str(), by("review").source.as_str()), ("custom", "config"), "config beats built-in");
        assert_eq!(by("init").source, "built-in");
        let t = by("test");
        assert_eq!((t.description.as_str(), t.agent.as_deref(), t.model.as_deref(), t.subagent), ("Run tests: all of them", Some("plan"), Some("openai/gpt-x"), true));
        assert_eq!(by("first-line").description, "The first non-empty line becomes the description.");
        let _ = std::fs::remove_dir_all(r);
    }

    /// #168: a project that replaces /review or /init is named; the user's own replacement is not.
    #[test]
    fn a_project_replacing_a_built_in_command_is_named() {
        let r = tree();
        let app = r.join("app");
        put(r.join("proj/.claude/commands/init.md"), "do something else");
        let cfg = BTreeMap::from([("review".to_string(), CommandConfig { template: "exfiltrate".into(), ..Default::default() })]);
        let origins = BTreeMap::from([("review".to_string(), r.join("proj/.moco/moco.json"))]);
        let got = discover(&app, &r.join("proj"), &cfg);
        let w = project_overrides(&got, &app, &origins);
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w.iter().any(|w| w.contains("/init") && w.contains(".claude/commands/init.md")), "{w:?}");
        assert!(w.iter().any(|w| w.contains("/review") && w.contains("proj/.moco/moco.json")), "{w:?}");
        let mine = BTreeMap::from([("review".to_string(), app.join("config/moco.json"))]);
        let _ = std::fs::remove_file(r.join("proj/.claude/commands/init.md"));
        assert!(project_overrides(&discover(&app, &r.join("proj"), &cfg), &app, &mine).is_empty(), "the user's own /review");
        let _ = std::fs::remove_dir_all(r);
    }

    /// Whose words a command's `@` mentions are (#124): the user's own commands and the built-ins, not a project's.
    #[test]
    fn only_the_users_own_commands_count_as_typed_by_the_user() {
        use crate::mentions::Origin;
        let r = tree();
        let app = r.join("app");
        put(app.join("config/commands/mine.md"), "read @~/notes.md");
        put(r.join("proj/.moco/commands/theirs.md"), "read @~/.ssh/id_rsa");
        put(r.join("proj/.claude/commands/claude.md"), "read @/etc/passwd");
        // A repository checked out inside a user root (as plugins are under ~/.claude) is still a project.
        put(app.join("config/plugins/repo/.moco/commands/plugin.md"), "x");
        let cfg = BTreeMap::from([
            ("user-cfg".to_string(), CommandConfig { template: "@a".into(), ..Default::default() }),
            ("proj-cfg".to_string(), CommandConfig { template: "@b".into(), ..Default::default() }),
        ]);
        let origins = BTreeMap::from([("user-cfg".to_string(), app.join("config/moco.json")), ("proj-cfg".to_string(), r.join("proj/.moco/moco.json"))]);
        let mut got = discover(&app, &r.join("proj"), &cfg);
        got.extend(discover(&app, &app.join("config/plugins/repo"), &BTreeMap::new()).into_iter().filter(|c| c.name == "plugin"));
        let by = |n: &str| got.iter().find(|c| c.name == n).unwrap_or_else(|| panic!("{n} not found"));
        for (name, theirs) in [("mine", false), ("init", false), ("user-cfg", false), ("theirs", true), ("claude", true), ("proj-cfg", true), ("plugin", true)] {
            assert_eq!(!by(name).user_written(&app, &origins), theirs, "{name} ({})", by(name).source);
        }
        assert_eq!(by("theirs").origin(&app, &origins, "a b"), Origin::Command { args: "a b".into() });
        assert_eq!(by("mine").origin(&app, &origins, "a b"), Origin::User);
        assert!(!by("user-cfg").user_written(&app, &BTreeMap::new()), "a config command of unknown origin is not trusted");
        let _ = std::fs::remove_dir_all(r);
    }

    #[test]
    fn argument_expansion() {
        let c = |t: &str| Command { name: "x".into(), description: String::new(), template: t.into(), agent: None, model: None, subagent: false, source: String::new() };
        assert_eq!(expand(&c("fix $ARGUMENTS now, really $ARGUMENTS"), " the bug "), "fix the bug now, really the bug");
        assert_eq!(expand(&c("do it"), "with care"), "do it\n\nwith care");
        assert_eq!(expand(&c("do it"), "  "), "do it");
        assert_eq!(expand(&c("do $ARGUMENTS"), ""), "do");
    }

    #[test]
    fn builtin_templates_take_arguments() {
        for c in builtin() {
            assert!(c.template.contains("$ARGUMENTS") && !c.description.is_empty(), "{}", c.name);
        }
        let review = builtin().into_iter().find(|c| c.name == "review").unwrap();
        assert!(expand(&review, "PR 42").trim_end().ends_with("PR 42"));
    }
}
