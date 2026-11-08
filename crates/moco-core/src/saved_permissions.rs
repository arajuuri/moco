//! Listing and removing the "always allow" rules saved for a project: `/permissions` in both chats and the
//! `moco permissions` subcommand share this so they number and match rules the same way.
use crate::permission::Rule;
use crate::store::{Store, StoreError};

pub const USAGE: &str = "remove <number> | remove <action> <pattern> | clear";

/// The saved rules as numbered lines, in the order `remove <number>` counts them. `cmd` is how the user runs the
/// command (`/permissions` or `moco permissions`), for the hints.
pub fn listing(rules: &[Rule], cmd: &str) -> Vec<String> {
    let mut out = vec![format!("saved \"always allow\" rules for this project: {}", rules.len())];
    for (i, r) in rules.iter().enumerate() {
        out.push(format!("  {:>2}. {:<20} {}", i + 1, r.action, r.resource));
    }
    if !rules.is_empty() {
        out.push(format!("  ({cmd} remove <number> forgets one, {cmd} clear forgets all)"));
    }
    out
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The arguments did not make sense; the message says why.
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Runs `remove ...` or `clear` (the text after `cmd`) for `project_id` and says what changed.
pub fn manage(store: &Store, project_id: &str, cmd: &str, args: &str) -> Result<Vec<String>, Error> {
    let args = args.trim();
    let (verb, rest) = args.split_once(char::is_whitespace).map(|(v, r)| (v, r.trim())).unwrap_or((args, ""));
    match verb {
        "clear" if rest.is_empty() => {
            let n = store.clear_saved_permissions(project_id)?;
            Ok(vec![format!("removed {n} saved \"always allow\" rule{} for this project", if n == 1 { "" } else { "s" })])
        }
        "remove" | "rm" if !rest.is_empty() => {
            let saved = store.saved_permissions(project_id)?;
            let rule = match rest.parse::<usize>() {
                Ok(n) => match n.checked_sub(1).and_then(|i| saved.get(i)) {
                    Some(r) => r.clone(),
                    None if saved.is_empty() => return Err(Error::Usage("there are no saved \"always allow\" rules for this project".into())),
                    None => return Err(Error::Usage(format!("no saved rule number {n}; the project has {} (see {cmd})", saved.len()))),
                },
                Err(_) => {
                    let Some((action, pattern)) = rest.split_once(char::is_whitespace) else {
                        return Err(Error::Usage(format!("usage: {cmd} {USAGE}")));
                    };
                    let (action, pattern) = (action.trim(), pattern.trim());
                    match saved.iter().find(|r| r.action == action && r.resource == pattern) {
                        Some(r) => r.clone(),
                        None => return Err(Error::Usage(format!("no saved rule \"{action} {pattern}\" for this project (see {cmd})"))),
                    }
                }
            };
            store.remove_saved_permission(project_id, &rule.action, &rule.resource)?;
            Ok(vec![format!("removed saved rule: {} {}; it asks again from the next request", rule.action, rule.resource)])
        }
        _ => Err(Error::Usage(format!("usage: {cmd} {USAGE}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::Effect;

    fn store() -> (Store, String) {
        let dir = std::env::temp_dir().join(crate::id::new_id("moco-saved"));
        let s = Store::open(&dir.join("moco.db")).unwrap();
        let p = s.project_for(&dir.to_string_lossy()).unwrap();
        (s, p)
    }

    #[test]
    fn rules_are_removed_by_number_or_by_action_and_pattern_and_only_in_their_project() {
        let (s, p) = store();
        s.save_permission(&p, "shell", "osascript *").unwrap();
        s.save_permission(&p, "shell", "cargo test *").unwrap();
        s.save_permission(&p, "edit", "*").unwrap();
        s.save_permission("prj_other", "shell", "osascript *").unwrap();
        let lines = listing(&s.saved_permissions(&p).unwrap(), "/permissions");
        assert!(lines[1].contains("1.") && lines[1].contains("osascript *"), "{lines:?}");

        let out = manage(&s, &p, "/permissions", "remove 1").unwrap();
        assert!(out[0].contains("shell osascript *"), "{out:?}");
        manage(&s, &p, "/permissions", "  remove   shell   cargo test * ").unwrap();
        assert_eq!(s.saved_permissions(&p).unwrap(), [Rule::new("edit", "*", Effect::Allow)]);
        assert_eq!(s.saved_permissions("prj_other").unwrap().len(), 1, "other projects keep theirs");

        assert!(matches!(manage(&s, &p, "/permissions", "remove 5"), Err(Error::Usage(m)) if m.contains("no saved rule number 5")));
        assert!(matches!(manage(&s, &p, "/permissions", "remove 0"), Err(Error::Usage(_))));
        assert!(matches!(manage(&s, &p, "/permissions", "remove shell nope"), Err(Error::Usage(m)) if m.contains("shell nope")));
        assert!(matches!(manage(&s, &p, "/permissions", "remove"), Err(Error::Usage(_))));
        assert!(matches!(manage(&s, &p, "/permissions", "frobnicate"), Err(Error::Usage(_))));
        assert_eq!(s.saved_permissions(&p).unwrap().len(), 1, "failed commands change nothing");
    }

    #[test]
    fn clear_forgets_every_rule_of_the_project() {
        let (s, p) = store();
        s.save_permission(&p, "shell", "a *").unwrap();
        s.save_permission(&p, "shell", "b *").unwrap();
        s.save_permission("prj_other", "shell", "a *").unwrap();
        assert_eq!(manage(&s, &p, "/permissions", "clear").unwrap(), ["removed 2 saved \"always allow\" rules for this project"]);
        assert!(s.saved_permissions(&p).unwrap().is_empty());
        assert_eq!(s.saved_permissions("prj_other").unwrap().len(), 1);
        assert!(matches!(manage(&s, &p, "/permissions", "remove 1"), Err(Error::Usage(m)) if m.contains("no saved")));
        assert!(matches!(manage(&s, &p, "/permissions", "clear all"), Err(Error::Usage(_))));
    }
}
