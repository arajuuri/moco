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
}

impl Rule {
    pub fn new(action: &str, resource: &str, effect: Effect) -> Self {
        Rule { action: action.into(), resource: resource.into(), effect }
    }
}

/// Last matching rule wins; no match means `Ask`.
pub fn evaluate(action: &str, resource: &str, rules: &[Rule]) -> Effect {
    rules
        .iter()
        .rev()
        .find(|r| wildcard::matches(action, &r.action) && wildcard::matches(resource, &r.resource))
        .map(|r| r.effect)
        .unwrap_or(Effect::Ask)
}

/// Default ruleset every new agent starts from.
pub fn default_rules() -> Vec<Rule> {
    vec![
        Rule::new("*", "*", Effect::Allow),
        Rule::new("read", "*.env", Effect::Ask),
        Rule::new("read", "*.env.*", Effect::Ask),
        Rule::new("read", "*.env.example", Effect::Allow),
        // A search sends your words to a third party: ask first (then "always" remembers it for the project).
        Rule::new("websearch", "*", Effect::Ask),
    ]
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
