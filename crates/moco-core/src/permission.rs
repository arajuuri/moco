//! Permission rule model and evaluation (docs/05-permissions.md).
use crate::wildcard;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Deny,
    Ask,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub action: String,
    pub resource: String,
    pub effect: Effect,
    /// An `ask` that is only a default (the mode's catch-all, the careful preset's `shell *`), which the command
    /// classifier may upgrade to allow. Never set by config: an `ask` the user wrote is final.
    #[serde(skip)]
    pub soft: bool,
}

impl Rule {
    pub fn new(action: &str, resource: &str, effect: Effect) -> Self {
        Rule { action: action.into(), resource: resource.into(), effect, soft: false }
    }
    /// A default `ask` that the command classifier may relax (see `soft`).
    pub fn soft_ask(action: &str, resource: &str) -> Self {
        Rule { soft: true, ..Rule::new(action, resource, Effect::Ask) }
    }
}

fn last_match<'a>(action: &str, resource: &str, rules: &'a [Rule]) -> Option<&'a Rule> {
    rules.iter().rev().find(|r| wildcard::matches(action, &r.action) && wildcard::matches(resource, &r.resource))
}

/// Last matching rule wins; no match means `Ask`.
pub fn evaluate(action: &str, resource: &str, rules: &[Rule]) -> Effect {
    last_match(action, resource, rules).map(|r| r.effect).unwrap_or(Effect::Ask)
}

/// True when every `ask` on these resources is only a default (a soft rule, or no rule at all), so the command
/// classifier may decide instead of the user. One explicit `ask` (or a `deny`) among them makes it false.
pub fn only_default_asks(action: &str, resources: &[String], configured: &[Rule], saved: &[Rule]) -> bool {
    let all: Vec<Rule> = configured.iter().chain(saved).cloned().collect();
    let mut asks = false;
    for r in resources {
        match last_match(action, r, &all) {
            Some(rule) if rule.effect == Effect::Deny => return false,
            Some(rule) if rule.effect == Effect::Ask => {
                if !rule.soft {
                    return false;
                }
                asks = true;
            }
            Some(_) => {}
            None => asks = true,
        }
    }
    asks
}

/// How much moco asks before it acts. `Manual` is the default; `Auto` allows what the default ruleset allows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PermissionMode {
    #[default]
    Manual,
    Auto,
}

impl PermissionMode {
    pub fn parse(s: &str) -> Option<PermissionMode> {
        match s {
            "manual" => Some(PermissionMode::Manual),
            "auto" => Some(PermissionMode::Auto),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            PermissionMode::Manual => "manual",
            PermissionMode::Auto => "auto",
        }
    }
    /// What the mode means to the user, for status messages.
    pub fn describe(self) -> &'static str {
        match self {
            PermissionMode::Manual => "asks before changing files, running commands, fetching the web or starting subagents",
            PermissionMode::Auto => "runs tools without asking, except where your permission rules ask",
        }
    }
    /// The mode Shift-Tab switches to.
    pub fn toggled(self) -> PermissionMode {
        match self {
            PermissionMode::Manual => PermissionMode::Auto,
            PermissionMode::Auto => PermissionMode::Manual,
        }
    }
}

/// The ruleset every agent starts from in a mode. Config rules come after it and win (last match wins).
pub fn mode_rules(mode: PermissionMode) -> Vec<Rule> {
    match mode {
        PermissionMode::Auto => default_rules(),
        // Ask for everything (a hard ask: nothing here is ever decided automatically, the classifier is for auto mode
        // only), then allow what only reads or plans: a prompt for every read would make the mode unusable.
        PermissionMode::Manual => {
            let mut rules = vec![Rule::new("*", "*", Effect::Ask)];
            rules.extend([
                Rule::new("read", "*", Effect::Allow),
                Rule::new("glob", "*", Effect::Allow),
                Rule::new("grep", "*", Effect::Allow),
                Rule::new("question", "*", Effect::Allow),
                Rule::new("todowrite", "*", Effect::Allow),
                Rule::new("skill", "*", Effect::Allow),
            ]);
            rules.extend(default_rules().into_iter().filter(|r| r.action == "read" && r.resource.starts_with("*.env")));
            rules.push(Rule::new("websearch", "*", Effect::Ask));
            rules
        }
    }
}

/// Ruleset of `Auto` mode: allow everything, except `.env` reads and web searches, which ask.
pub fn default_rules() -> Vec<Rule> {
    vec![
        Rule::new("*", "*", Effect::Allow),
        Rule::new("read", "*.env", Effect::Ask),
        Rule::new("read", "*.env.*", Effect::Ask),
        Rule::new("read", "*.env.example", Effect::Allow),
        // A search sends your words to a third party: ask first (then "always" remembers it for the project).
        Rule::new("websearch", "*", Effect::Ask),
        // Running outside the shell sandbox is never covered by the blanket allow.
        Rule::new("shell.unsandboxed", "*", Effect::Ask),
        // Nor is a fetch from this machine or its local network (`tools/webfetch.rs`, #115).
        Rule::new("webfetch.private", "*", Effect::Ask),
    ]
}

/// Shell commands the careful preset runs without asking. Their options that run programs or write files
/// (`git log --output=`, `rg --pre`, `cargo --config`...) make a command opaque (`shellparse`), the files they read
/// get the external-directory check, and reading `.env` asks, so each of these only reads or builds (issue #117).
/// Building and testing still run the project's own code (`build.rs`, tests); the sandbox is what contains that.
pub const CAREFUL_SHELL_ALLOW: &[&str] = &[
    "ls", "cat", "head", "tail", "wc", "rg", "grep", "git status", "git diff", "git log", "git show", "cargo check", "cargo test", "cargo build",
];

/// Paths in the project that the careful preset asks before the edit tools change: files that git, moco and other
/// agents or editors run code from or load as configuration on their own (`.git/hooks`, and `.git/config`, whose
/// `core.fsmonitor` every `git status` runs), and, for a project in the home folder, shell startup files and other
/// places in `$HOME` that run code (compare `sandbox.rs`). Paths outside the project ask through `external_directory`.
const CAREFUL_EDIT_ASK: &[&str] = &[
    ".git", ".moco", "moco.json", "moco.jsonc", ".claude", ".agents", ".vscode", ".profile", ".bashrc", ".bash_profile", ".bash_login",
    ".bash_logout", ".zshrc", ".zshenv", ".zprofile", ".zlogin", ".zlogout", ".cshrc", ".tcshrc", ".login", ".logout", ".config/fish",
    ".config/git", ".gitconfig", ".ssh", "Library/LaunchAgents",
];

/// Rules of a `permission_preset`, placed after the mode rules and before the user's own `permissions`.
/// `trusted` adds nothing; `careful` asks for shell commands (except read-only and build ones), web fetches, edits
/// outside the project and edits of the project's git, moco and agent config (`CAREFUL_EDIT_ASK`), and always asks
/// for `git push`, `rm -rf` and `sudo`.
pub fn preset_rules(name: &str) -> Option<Vec<Rule>> {
    match name {
        "trusted" => Some(vec![]),
        "careful" => {
            let mut rules = vec![Rule::soft_ask("shell", "*")];
            for c in CAREFUL_SHELL_ALLOW {
                rules.push(Rule::new("shell", &format!("{c} *"), Effect::Allow));
            }
            // A variable can expand to an option the rules never saw (`X=--output=x; git log $X`): the classifier,
            // which sees the whole command line, decides instead of the allow.
            for c in CAREFUL_SHELL_ALLOW {
                rules.push(Rule::soft_ask("shell", &format!("{c} *$*")));
            }
            for c in ["git push *", "rm -rf *", "rm -fr *", "sudo *"] {
                rules.push(Rule::new("shell", c, Effect::Ask));
            }
            rules.extend([
                Rule::new("webfetch", "*", Effect::Ask),
                Rule::new("edit", "*", Effect::Allow),
                Rule::new("external_directory", "*", Effect::Ask),
                Rule::new("shell.unsandboxed", "*", Effect::Ask),
            ]);
            for p in CAREFUL_EDIT_ASK {
                for r in [p.to_string(), format!("{p}/*"), format!("*/{p}"), format!("*/{p}/*")] {
                    rules.push(Rule::new("edit", &r, Effect::Ask));
                }
            }
            // A file named like an option (`--pre=sh`) becomes one when a glob in a command expands to it.
            rules.extend([Rule::new("edit", "-*", Effect::Ask), Rule::new("edit", "*/-*", Effect::Ask)]);
            Some(rules)
        }
        _ => None,
    }
}

/// `configured` = agent rules followed by session rules. `saved` = user "always" rules (all allow).
/// 1. Any resource denied by configured rules -> Deny (saved rules cannot override a deny).
/// 2. Otherwise evaluate against configured + saved; any Ask -> Ask, else Allow.
pub fn decide(action: &str, resources: &[String], configured: &[Rule], saved: &[Rule]) -> Effect {
    if resources.iter().any(|r| evaluate(action, r, configured) == Effect::Deny) {
        return Effect::Deny;
    }
    let all: Vec<Rule> = configured.iter().chain(saved).cloned().collect();
    if resources.iter().any(|r| evaluate(action, r, &all) == Effect::Ask) {
        Effect::Ask
    } else {
        Effect::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Effect::*;

    fn res(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn last_match_wins_and_default_ask() {
        assert_eq!(evaluate("read", "x", &[]), Ask);
        let rules = [Rule::new("*", "*", Deny), Rule::new("read", "*", Allow)];
        assert_eq!(evaluate("read", "a.txt", &rules), Allow);
        assert_eq!(evaluate("edit", "a.txt", &rules), Deny);
    }

    #[test]
    fn unsandboxed_shell_asks_even_in_auto_mode() {
        let auto = mode_rules(PermissionMode::Auto);
        assert_eq!(evaluate("shell", "ls", &auto), Effect::Allow);
        assert_eq!(evaluate("shell.unsandboxed", "ls", &auto), Effect::Ask);
    }

    #[test]
    fn default_ruleset_env_files() {
        let d = default_rules();
        assert_eq!(evaluate("read", "src/main.rs", &d), Allow);
        assert_eq!(evaluate("read", ".env", &d), Ask);
        assert_eq!(evaluate("read", "app/.env.local", &d), Ask);
        assert_eq!(evaluate("read", ".env.example", &d), Allow);
        assert_eq!(evaluate("shell", "rm -rf /", &d), Allow);
        assert_eq!(evaluate("websearch", "rust ureq timeout", &d), Ask, "queries leave the machine: ask first");
    }

    #[test]
    fn tab_toggles_between_the_two_modes() {
        assert_eq!(PermissionMode::Manual.toggled(), PermissionMode::Auto);
        assert_eq!(PermissionMode::Auto.toggled(), PermissionMode::Manual);
        assert_eq!(PermissionMode::parse("auto"), Some(PermissionMode::Auto));
        assert_eq!(PermissionMode::parse("Auto"), None, "names are lowercase in config and flags");
    }

    #[test]
    fn manual_asks_for_changes_and_auto_allows_them() {
        let manual = mode_rules(PermissionMode::Manual);
        assert_eq!(evaluate("shell", "ls", &manual), Ask);
        assert_eq!(evaluate("edit", "a.rs", &manual), Ask);
        assert_eq!(evaluate("subagent", "general", &manual), Ask);
        assert_eq!(evaluate("read", "src/main.rs", &manual), Allow);
        assert_eq!(evaluate("grep", "*", &manual), Allow);
        assert_eq!(evaluate("read", ".env", &manual), Ask);
        let auto = mode_rules(PermissionMode::Auto);
        assert_eq!(evaluate("shell", "ls", &auto), Allow);
        assert_eq!(evaluate("edit", "a.rs", &auto), Allow);
        assert_eq!(PermissionMode::default(), PermissionMode::Manual);
    }

    #[test]
    fn only_default_asks_are_open_to_the_classifier() {
        let manual = mode_rules(PermissionMode::Manual);
        assert!(!only_default_asks("shell", &res(&["ls -la"]), &manual, &[]), "manual mode is fully manual");
        assert!(only_default_asks("shell", &res(&["ls"]), &[], &[]), "no rule at all is a default ask");
        let careful: Vec<Rule> = mode_rules(PermissionMode::Auto).into_iter().chain(preset_rules("careful").unwrap()).collect();
        assert!(only_default_asks("shell", &res(&["make test"]), &careful, &[]));
        assert!(!only_default_asks("shell", &res(&["git push origin x"]), &careful, &[]), "the always-ask list is final");
        assert!(!only_default_asks("shell", &res(&["make", "sudo rm x"]), &careful, &[]));
        assert!(!only_default_asks("shell", &res(&["cat a"]), &careful, &[]), "nothing asks: nothing to classify");
        // A rule the user wrote is explicit even when it looks like the default one.
        let mut user = careful.clone();
        user.push(Rule::new("shell", "*", Effect::Ask));
        assert!(!only_default_asks("shell", &res(&["make"]), &user, &[]));
        user.push(Rule::new("shell", "make", Effect::Deny));
        assert!(!only_default_asks("shell", &res(&["make"]), &user, &[]));
        let from_json: Rule = serde_json::from_str(r#"{"action":"shell","resource":"*","effect":"ask","soft":true}"#).unwrap();
        assert!(!from_json.soft, "config cannot mark a rule soft");
    }

    #[test]
    fn careful_preset_allows_reads_and_builds_but_not_what_their_options_hide() {
        // Issue #117.
        let careful: Vec<Rule> = mode_rules(PermissionMode::Auto).into_iter().chain(preset_rules("careful").unwrap()).collect();
        let shell = |c: &str| {
            let p = crate::tools::shellparse::parse(c);
            let r: Vec<String> = p.commands.iter().map(|c| c.text.clone()).collect();
            (if p.opaque { Ask } else { decide("shell", &r, &careful, &[]) }, p.opaque, only_default_asks("shell", &r, &careful, &[]))
        };
        for c in [
            "git status", "git diff --stat", "git log --oneline -5", "git log -p -1 --format='%h %s'", "git show HEAD", "ls -la", "cat src/main.rs",
            "head -n 20 Cargo.toml", "wc -l src/*.rs", "grep -rn TODO src", "rg -n --glob '*.rs' unwrap", "cargo test -p moco-core", "cargo build --release",
            "CARGO_TARGET_DIR=/tmp/t cargo check",
        ] {
            assert_eq!(shell(c), (Allow, false, false), "{c}");
        }
        // Options that write files or run programs make the command opaque: it asks, and the classifier never sees it.
        for c in ["git log -1 --format=%B --output=/tmp/x", "git diff --ext-diff", "rg --pre sh x", "rg x *", "cargo test --config 'build.rustc-wrapper=\"x\"'", "cat .env", "RIPGREP_CONFIG_PATH=x rg y"] {
            assert_eq!(shell(c).0, Ask, "{c}");
            assert!(shell(c).1, "{c}");
        }
        // A variable could expand to an option: a default ask, which the classifier (seeing the whole line) may allow.
        for c in ["X=--output=/tmp/x; git log $X", "cargo test \"$T\"", "grep -n 'x$' f"] {
            assert_eq!(shell(c), (Ask, false, true), "{c}");
        }
        assert_eq!(shell("git push origin x"), (Ask, false, false), "the always-ask list still wins");

        // Edits of what git, moco and other agents run or load ask; ordinary project files do not.
        for p in [
            ".git/config", ".git/hooks/pre-commit", ".git", "sub/.git/config", ".moco/moco.json", "moco.json", "moco.jsonc", ".claude/settings.json",
            ".agents/skills/x/SKILL.md", ".vscode/tasks.json", ".zshrc", ".config/fish/config.fish", ".ssh/authorized_keys", "Library/LaunchAgents/x.plist",
            "--pre=sh", "src/-x",
        ] {
            assert_eq!(evaluate("edit", p, &careful), Ask, "{p}");
        }
        for p in ["src/main.rs", ".gitignore", ".github/workflows/ci.yml", "AGENTS.md", "docs/moco.json.md", "a-b.rs", "src/x-y/z.rs", "build.rs"] {
            assert_eq!(evaluate("edit", p, &careful), Allow, "{p}");
        }
        assert_eq!(evaluate("edit", ".git/config", &mode_rules(PermissionMode::Auto)), Allow, "only the preset asks");
    }

    #[test]
    fn saved_rules_cannot_override_deny() {
        let configured = [Rule::new("*", "*", Allow), Rule::new("shell", "rm *", Deny)];
        let saved = [Rule::new("shell", "rm *", Allow)];
        assert_eq!(decide("shell", &res(&["rm -rf x"]), &configured, &saved), Deny);
    }

    #[test]
    fn saved_rules_resolve_ask() {
        let configured = default_rules();
        let saved = [Rule::new("read", "*.env", Allow)];
        assert_eq!(decide("read", &res(&[".env"]), &configured, &[]), Ask);
        assert_eq!(decide("read", &res(&[".env"]), &configured, &saved), Allow);
    }

    #[test]
    fn multi_resource_any_ask_asks_any_deny_denies() {
        let configured = [Rule::new("*", "*", Allow), Rule::new("shell", "git push *", Ask), Rule::new("shell", "rm *", Deny)];
        assert_eq!(decide("shell", &res(&["ls", "git push origin x"]), &configured, &[]), Ask);
        assert_eq!(decide("shell", &res(&["git push a", "rm x"]), &configured, &[]), Deny);
        assert_eq!(decide("shell", &res(&["ls", "pwd"]), &configured, &[]), Allow);
    }
}
