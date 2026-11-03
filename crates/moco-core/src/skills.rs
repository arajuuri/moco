//! Skills (docs/08 "Skills"): a directory with `SKILL.md` (frontmatter `name` and `description`, body = the
//! instructions) plus optional files. The model sees the list and loads one with the `skill` tool.
use crate::markdown;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Directory name; what `skill` and `@mentions` refer to.
    pub id: String,
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    pub body: String,
}

/// The user's home. Unit tests get a directory that does not exist, so skills, commands and instructions installed
/// in the real `~/.claude` or `~/.agents` cannot leak into discovery tests.
fn home() -> PathBuf {
    if cfg!(test) {
        return std::env::temp_dir().join("moco-test-no-home");
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

/// Config roots, lowest priority first: `~/.claude`, `~/.agents`, `<app dir>/config`, then for every directory
/// from the filesystem root down to the working directory its `.claude`, `.agents` and `.moco`.
/// (`.claude` and `.agents` are read for compatibility with skills and commands written for other tools.)
pub fn config_roots(app_dir: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut roots = vec![home().join(".claude"), home().join(".agents"), app_dir.join("config")];
    let mut ancestors: Vec<&Path> = cwd.ancestors().collect();
    ancestors.reverse();
    for a in ancestors {
        roots.extend([a.join(".claude"), a.join(".agents"), a.join(".moco")]);
    }
    roots
}

fn expand(p: &str, cwd: &Path) -> PathBuf {
    let p = match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(p),
    };
    if p.is_absolute() { p } else { cwd.join(p) }
}

fn load_dir(dir: &Path) -> Option<Skill> {
    let text = std::fs::read_to_string(dir.join("SKILL.md")).ok()?;
    let doc = markdown::parse(&text);
    let id = dir.file_name()?.to_str()?.to_string();
    let description = doc.get("description")?.to_string(); // a skill without a description cannot be matched to a task
    Some(Skill { name: doc.get("name").unwrap_or(&id).to_string(), id, description, dir: dir.to_path_buf(), body: doc.body })
}

/// All skills, sorted by id (a stable order keeps the prompt cache valid). A later root overrides an earlier one with the same id.
/// `extra` are the `skills` paths from config: a directory of skills, or a single skill directory.
pub fn discover(app_dir: &Path, cwd: &Path, extra: &[String]) -> Vec<Skill> {
    let mut found: std::collections::BTreeMap<String, Skill> = Default::default();
    let mut scan = |dir: &Path| {
        if let Some(s) = load_dir(dir) {
            found.insert(s.id.clone(), s);
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut subs: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect();
        subs.sort();
        for p in subs {
            if let Some(s) = load_dir(&p) {
                found.insert(s.id.clone(), s);
            }
        }
    };
    for root in config_roots(app_dir, cwd) {
        scan(&root.join("skills"));
    }
    for e in extra {
        scan(&expand(e, cwd));
    }
    found.into_values().collect()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The block added to the system prompt; empty when there are no skills.
pub fn render_list(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "Skills provide specialized instructions and workflows for specific tasks.\nWhen a task matches a skill's description, load the skill with the `skill` tool before you start, then follow it.\n<available_skills>\n",
    );
    for k in skills {
        s.push_str(&format!("  <skill><id>{}</id><name>{}</name><description>{}</description></skill>\n", esc(&k.id), esc(&k.name), esc(&k.description)));
    }
    s.push_str("</available_skills>\n");
    s
}

/// What the model receives when a skill is loaded (by the `skill` tool or an `@skill` mention).
pub fn render_content(skill: &Skill) -> String {
    let mut files: Vec<String> = vec![];
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>, depth: usize) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if out.len() >= 10 {
                return;
            }
            if p.is_dir() && depth < 3 {
                walk(root, &p, out, depth + 1);
            } else if p.is_file() && p.file_name().is_some_and(|n| n != "SKILL.md") {
                if let Ok(rel) = p.strip_prefix(root) {
                    out.push(rel.display().to_string());
                }
            }
        }
    }
    walk(&skill.dir, &skill.dir, &mut files, 0);
    let list = files.iter().map(|f| format!("<file>{}</file>", esc(f))).collect::<Vec<_>>().join("\n");
    format!(
        "<skill_content name=\"{}\">\n# Skill: {}\n\n{}\n\nBase directory for this skill: {}\nRelative paths mentioned in this skill (scripts, references, templates) are relative to that directory.\n\n<skill_files>\n{}\n</skill_files>\n</skill_content>",
        esc(&skill.name),
        skill.name,
        skill.body.trim_end(),
        skill.dir.display(),
        list
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> PathBuf {
        let root = std::env::temp_dir().join(crate::id::new_id("moco-skills"));
        std::fs::create_dir_all(root.join("app/config")).unwrap();
        std::fs::create_dir_all(root.join("proj/sub")).unwrap();
        root
    }
    fn skill(dir: &Path, id: &str, desc: Option<&str>, body: &str) {
        let d = dir.join(id);
        std::fs::create_dir_all(&d).unwrap();
        let head = desc.map(|d| format!("---\nname: {id}-name\ndescription: {d}\n---\n")).unwrap_or_default();
        std::fs::write(d.join("SKILL.md"), format!("{head}{body}")).unwrap();
    }

    #[test]
    fn discovery_priority_compat_dirs_and_validity() {
        let r = tree();
        skill(&r.join("app/config/skills"), "shared", Some("from the global config"), "global body");
        skill(&r.join("app/config/skills"), "only-global", Some("g"), "x");
        skill(&r.join("proj/.claude/skills"), "shared", Some("from .claude"), "claude body");
        skill(&r.join("proj/.moco/skills"), "shared", Some("from .moco"), "moco body");
        skill(&r.join("proj/sub/.agents/skills"), "deep", Some("agents dir"), "x");
        skill(&r.join("proj/.moco/skills"), "no-description", None, "has no frontmatter");
        skill(&r.join("extra"), "from-config", Some("extra path"), "x");
        std::fs::write(r.join("proj/.moco/skills/stray-file.md"), "ignored").unwrap();
        let got = discover(&r.join("app"), &r.join("proj/sub"), &["../../extra".into()]);
        let ids: Vec<_> = got.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["deep", "from-config", "only-global", "shared"], "sorted; invalid skills skipped");
        let shared = got.iter().find(|s| s.id == "shared").unwrap();
        assert_eq!((shared.description.as_str(), shared.body.as_str(), shared.name.as_str()), ("from .moco", "moco body", "shared-name"), ".moco beats .claude beats global");
        // A skill directory can also be given directly.
        let one = discover(&r.join("app"), &r.join("proj"), &[r.join("extra/from-config").to_string_lossy().into_owned()]);
        assert!(one.iter().any(|s| s.id == "from-config"));
        let _ = std::fs::remove_dir_all(r);
    }

    #[test]
    fn list_and_content_rendering() {
        let r = tree();
        skill(&r.join("s"), "pdf", Some("Fill <forms> & more"), "Step 1: run the script.\n");
        std::fs::create_dir_all(r.join("s/pdf/scripts")).unwrap();
        std::fs::write(r.join("s/pdf/scripts/fill.py"), "print()").unwrap();
        std::fs::write(r.join("s/pdf/REFERENCE.md"), "ref").unwrap();
        let skills = discover(&r.join("app"), &r.join("proj"), &[r.join("s").to_string_lossy().into_owned()]);
        let list = render_list(&skills);
        assert!(list.contains("<skill><id>pdf</id><name>pdf-name</name><description>Fill &lt;forms&gt; &amp; more</description></skill>"), "{list}");
        assert_eq!(render_list(&[]), "");
        let c = render_content(&skills[0]);
        assert!(c.starts_with("<skill_content name=\"pdf-name\">\n# Skill: pdf-name\n\nStep 1: run the script."), "{c}");
        assert!(c.contains(&format!("Base directory for this skill: {}", r.join("s/pdf").display())));
        assert!(c.contains("<file>REFERENCE.md</file>\n<file>scripts/fill.py</file>") && !c.contains("SKILL.md</file>"), "{c}");
        let _ = std::fs::remove_dir_all(r);
    }

    #[test]
    fn at_most_ten_sibling_files() {
        let r = tree();
        skill(&r, "many", Some("d"), "b");
        for i in 0..15 {
            std::fs::write(r.join(format!("many/f{i:02}.txt")), "x").unwrap();
        }
        let s = discover(&r.join("app"), &r.join("proj"), &[r.join("many").to_string_lossy().into_owned()]);
        assert_eq!(render_content(&s[0]).matches("<file>").count(), 10);
        let _ = std::fs::remove_dir_all(r);
    }
}
